//! A zero-dependency Rust driver for Frostlake, speaking the engine's HTTP
//! protocol against a running `DatabaseHttpServer`.
//!
//! ```no_run
//! use frostlake::{connect, Value};
//!
//! let mut conn = connect("frostlake://localhost:18082/MY_DB?schema=PUBLIC").unwrap();
//! let result = conn
//!     .execute("SELECT id, name FROM people WHERE id = ?", &[Value::Int(1)])
//!     .unwrap();
//! assert_eq!(result.get(0, "ID"), Some(&Value::Int(1)));
//! ```
//!
//! Parameters are inlined client-side (the protocol has no server-side
//! binding), with the same rules as Frostlake's other drivers. Integral NUMBER
//! cells arrive as `Value::Int(i128)` — exact through `NUMBER(38,0)` — and
//! DATE/TIMESTAMP*/BINARY cells as `Value::Date`/`Value::Timestamp`/
//! `Value::Bytes`.

mod http;
mod json;
#[cfg(test)]
mod test_support;

use std::fmt;

pub use json::Value;

#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    fn new(message: impl Into<String>) -> Error {
        Error { message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::new(format!("request failed: {e}"))
    }
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub data_type: Option<String>,
}

/// One statement's outcome: `rows` are indexed positionally and read by column
/// name via [`QueryResult::get`]; `row_count` counts them — except for DML,
/// whose single row holds the server's per-action counts and whose `row_count`
/// is the affected-row count.
#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub row_count: i64,
}

impl QueryResult {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name == name)
            .or_else(|| self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)))
    }

    pub fn get(&self, row: usize, column: &str) -> Option<&Value> {
        let index = self.column_index(column)?;
        self.rows.get(row)?.get(index)
    }
}

/// Connects and verifies the server is reachable via `GET /api/health`.
pub fn connect(dsn: &str) -> Result<Connection, Error> {
    let mut conn = Connection::new(dsn)?;
    conn.ping()?;
    Ok(conn)
}

#[derive(Debug)]
pub struct Connection {
    host: String,
    port: u16,
    session_id: Option<String>,
    auto_commit: bool,
    closed: bool,
    pending_use: Vec<String>,
}

impl Connection {
    fn new(dsn: &str) -> Result<Connection, Error> {
        let (scheme, rest) = dsn
            .split_once("://")
            .ok_or_else(|| Error::new("DSN must start with frostlake:// or http://"))?;
        // frostlake:// without an explicit port means the server default, 18082
        // (an http:// DSN keeps URL semantics, port 80) — like the other drivers.
        let default_port: u16 = if scheme.eq_ignore_ascii_case("frostlake") {
            18082
        } else if scheme.eq_ignore_ascii_case("http") {
            80
        } else {
            return Err(Error::new("DSN must start with frostlake:// or http://"));
        };
        // The query starts at the first '?' wherever it appears, so a DSN
        // without a path can still carry one: frostlake://host?schema=PUBLIC.
        let (location, query) = match rest.split_once('?') {
            Some((l, q)) => (l, Some(q)),
            None => (rest, None),
        };
        let (authority, database_part) = match location.split_once('/') {
            Some((a, d)) => (a, d),
            None => (location, ""),
        };
        if authority.is_empty() {
            return Err(Error::new("DSN is missing host[:port]"));
        }
        // IPv6 literals are bracketed, URL-style: frostlake://[::1]:18082/db.
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (inside, after) = bracketed
                .split_once(']')
                .ok_or_else(|| Error::new("unclosed '[' in DSN host"))?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse::<u16>().map_err(|_| Error::new(format!("invalid port '{p}'")))?,
                None if after.is_empty() => default_port,
                None => return Err(Error::new(format!("unexpected '{after}' after IPv6 host"))),
            };
            (inside.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (
                    h.to_string(),
                    p.parse::<u16>().map_err(|_| Error::new(format!("invalid port '{p}'")))?,
                ),
                None => (authority.to_string(), default_port),
            }
        };
        let database = percent_decode(database_part.trim_matches('/'), false)?;
        let mut schema = None;
        if let Some(q) = query {
            for pair in q.split('&') {
                if let Some((key, value)) = pair.split_once('=') {
                    if key.eq_ignore_ascii_case("schema") && !value.is_empty() {
                        schema = Some(percent_decode(value, true)?);
                        break;
                    }
                }
            }
        }
        let mut pending_use = Vec::new();
        if !database.is_empty() {
            pending_use.push(format!("USE DATABASE {}", quote_ident(&database)));
        }
        if let Some(schema) = schema {
            pending_use.push(format!("USE SCHEMA {}", quote_ident(&schema)));
        }
        Ok(Connection {
            host,
            port,
            session_id: None,
            auto_commit: true,
            closed: false,
            pending_use,
        })
    }

    fn ping(&mut self) -> Result<(), Error> {
        let response = http::request(&self.host, self.port, "GET", "/api/health", None)
            .map_err(|e| Error::new(format!("cannot reach {}:{}: {e}", self.host, self.port)))?;
        if response.status != 200 {
            return Err(Error::new(format!("server unhealthy: HTTP {}", response.status)));
        }
        Ok(())
    }

    /// Executes one statement, inlining `?` placeholders from `binds` in order.
    pub fn execute(&mut self, sql: &str, binds: &[Value]) -> Result<QueryResult, Error> {
        if self.closed {
            return Err(Error::new("connection is closed"));
        }
        // A failed USE stays queued, so every later statement keeps failing
        // instead of silently running against the server's default database.
        while !self.pending_use.is_empty() {
            let statement = self.pending_use[0].clone();
            self.round_trip(&statement)?;
            self.pending_use.remove(0);
        }
        let rendered = if binds.is_empty() {
            sql.to_string()
        } else {
            substitute(sql, binds)?
        };
        let out = self.round_trip(&rendered)?;
        Ok(shape_result(&out))
    }

    pub fn begin(&mut self) -> Result<(), Error> {
        self.auto_commit = false;
        // A BEGIN that fails opened no transaction, so autocommit stays on.
        if let Err(e) = self.execute("BEGIN", &[]) {
            self.auto_commit = true;
            return Err(e);
        }
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), Error> {
        self.execute("COMMIT", &[])?;
        self.auto_commit = true;
        Ok(())
    }

    pub fn rollback(&mut self) -> Result<(), Error> {
        self.execute("ROLLBACK", &[])?;
        self.auto_commit = true;
        Ok(())
    }

    pub fn close(&mut self) {
        self.closed = true;
    }

    fn round_trip(&mut self, sql: &str) -> Result<Value, Error> {
        let mut payload = format!(
            "{{\"sql\":\"{}\",\"autoCommit\":{}",
            json::escape(sql),
            self.auto_commit
        );
        if let Some(session_id) = &self.session_id {
            payload.push_str(&format!(",\"sessionId\":\"{}\"", json::escape(session_id)));
        }
        payload.push('}');
        // Failed statements answer with a non-2xx status AND the error payload in the body.
        let response = http::request(&self.host, self.port, "POST", "/api/execute", Some(&payload))?;
        let out = json::parse(&response.body).map_err(|e| {
            Error::new(format!(
                "HTTP {} with unreadable body: {e}, near `{}`",
                response.status,
                excerpt(&response.body, e.at)
            ))
        })?;
        if let Some(Value::Str(session_id)) = out.get("sessionId") {
            self.session_id = Some(session_id.clone());
        }
        if out.get("success").and_then(Value::as_bool) != Some(true) {
            // Request validation answers {"error": …} rather than errorMessage.
            let message = out
                .get("errorMessage")
                .and_then(Value::as_str)
                .or_else(|| out.get("error").and_then(Value::as_str))
                .unwrap_or("statement failed");
            return Err(Error::new(message));
        }
        Ok(out)
    }
}

/// Up to 40 bytes either side of `at`, cut on character boundaries.
fn excerpt(text: &str, at: usize) -> &str {
    let mut start = at.saturating_sub(40).min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = at.saturating_add(40).min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    &text[start..end]
}

fn shape_result(out: &Value) -> QueryResult {
    let result_set = out
        .get("resultSets")
        .and_then(Value::as_array)
        .and_then(|sets| sets.first());
    let Some(result_set) = result_set else {
        return QueryResult { columns: Vec::new(), rows: Vec::new(), row_count: 0 };
    };
    let columns: Vec<Column> = result_set
        .get("columns")
        .and_then(Value::as_array)
        .map(|cols| {
            cols.iter()
                .map(|c| Column {
                    name: c.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    data_type: c.get("dataType").and_then(Value::as_str).map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    let rows: Vec<Vec<Value>> = result_set
        .get("rows")
        .and_then(Value::as_array)
        .map(|raw_rows| {
            raw_rows
                .iter()
                .map(|raw| {
                    let cells = raw.as_array().unwrap_or(&[]);
                    columns
                        .iter()
                        .enumerate()
                        .map(|(i, column)| {
                            convert(cells.get(i).cloned().unwrap_or(Value::Null), &column.data_type)
                        })
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default();
    // DML answers with a single row of count columns: one for INSERT/DELETE,
    // several for MERGE and UPDATE. The row stays readable as the server sent
    // it — per-action MERGE counts included — and `row_count` sums the
    // affected-row counts the way the engine's JDBC driver does. The names are
    // matched exactly so an aliased look-alike ("number of rows once") counts
    // its rows, and the always-0 "number of multi-joined rows updated" UPDATE
    // column is recognized but, per Snowflake semantics, left out of the sum.
    let row_count = dml_count(&columns, &rows).unwrap_or(rows.len() as i64);
    QueryResult { columns, rows, row_count }
}

/// The affected-row count of a DML answer; `None` for any other result.
fn dml_count(columns: &[Column], rows: &[Vec<Value>]) -> Option<i64> {
    if rows.len() != 1 || columns.is_empty() {
        return None;
    }
    let mut total: i128 = 0;
    let mut any_summed = false;
    for (i, column) in columns.iter().enumerate() {
        let name = column.name.to_lowercase();
        if name == "number of rows inserted"
            || name == "number of rows updated"
            || name == "number of rows deleted"
        {
            any_summed = true;
            total += rows[0][i].as_i128().unwrap_or(0);
        } else if name != "number of multi-joined rows updated" {
            return None;
        }
    }
    any_summed.then_some(total as i64)
}

fn convert(value: Value, data_type: &Option<String>) -> Value {
    let Value::Str(text) = value else {
        return value;
    };
    match data_type.as_deref().unwrap_or("").to_ascii_uppercase().as_str() {
        "DATE" => Value::Date(text),
        "TIMESTAMP" | "TIMESTAMP_NTZ" | "TIMESTAMP_LTZ" | "TIMESTAMP_TZ" | "DATETIME" => {
            Value::Timestamp(text)
        }
        "BINARY" | "VARBINARY" => match decode_hex(&text) {
            Some(bytes) => Value::Bytes(bytes),
            None => Value::Str(text),
        },
        // NaN and the infinities have no JSON number, so they cross as text.
        "FLOAT" | "FLOAT4" | "FLOAT8" | "DOUBLE" | "DOUBLE PRECISION" | "REAL" => {
            match text.parse::<f64>() {
                Ok(f) => Value::Float(f),
                Err(_) => Value::Str(text),
            }
        }
        _ => Value::Str(text),
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Decodes URL `%XX` escapes (and, in query values, `+` as space), so a DSN can
/// name a database or schema containing spaces, slashes or `?`.
fn percent_decode(text: &str, form_encoded: bool) -> Result<String, Error> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let decoded = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| Error::new(format!("invalid %-escape in '{text}'")))?;
                out.push(decoded);
                i += 3;
            }
            b'+' if form_encoded => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| Error::new(format!("invalid UTF-8 after %-decoding '{text}'")))
}

fn quote_ident(name: &str) -> String {
    // A valid unquoted identifier passes through bare in either case — the
    // engine uppercases it, like Snowflake and like the JDBC driver's URL
    // handling. Anything else is quoted, preserving exact case.
    let bytes = name.as_bytes();
    let plain = match bytes.first() {
        Some(&first) => {
            (first.is_ascii_alphabetic() || first == b'_')
                && bytes[1..]
                    .iter()
                    .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
        }
        None => false,
    };
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

// -- client-side parameter binding -------------------------------------------

/// Replaces `?` placeholders with SQL literals, leaving placeholders inside
/// string literals (both `''` and `\'` escape), dollar-quoted `$$…$$` strings,
/// quoted identifiers and `--`/`//`/`/* */` comments untouched. Errs when the
/// bind count does not match the placeholder count, in either direction.
pub fn substitute(sql: &str, binds: &[Value]) -> Result<String, Error> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16 * binds.len());
    let mut plain = 0usize;
    let mut next = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' {
            i = skip_string(bytes, i);
        } else if c == b'"' {
            i = skip_quoted(bytes, i);
        } else if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            i = skip_line(bytes, i);
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i = find_block_comment_end(bytes, i + 2);
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i = skip_line(bytes, i);
        } else if c == b'$' && bytes.get(i + 1) == Some(&b'$') {
            i = skip_dollar_quoted(bytes, i + 2);
        } else if c == b'?' {
            out.push_str(&sql[plain..i]);
            let bind = binds
                .get(next)
                .ok_or_else(|| Error::new("not enough bind values for placeholders"))?;
            out.push_str(&format_literal(bind)?);
            next += 1;
            i += 1;
            plain = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&sql[plain..]);
    if next < binds.len() {
        return Err(Error::new(format!(
            "too many bind values: {} given, {} placeholder{}",
            binds.len(),
            next,
            if next == 1 { "" } else { "s" }
        )));
    }
    Ok(out)
}

fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut j = start + 1;
    while j < bytes.len() {
        if bytes[j] == b'\\' {
            j += 2; // backslash always escapes
        } else if bytes[j] == b'\'' {
            if bytes.get(j + 1) == Some(&b'\'') {
                j += 2;
            } else {
                return j + 1;
            }
        } else {
            j += 1;
        }
    }
    j.min(bytes.len())
}

fn skip_quoted(bytes: &[u8], start: usize) -> usize {
    let mut j = start + 1;
    while j < bytes.len() {
        if bytes[j] == b'"' {
            if bytes.get(j + 1) == Some(&b'"') {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    j
}

fn skip_line(bytes: &[u8], start: usize) -> usize {
    match bytes[start..].iter().position(|&b| b == b'\n') {
        Some(offset) => start + offset + 1,
        None => bytes.len(),
    }
}

fn find_block_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut j = start;
    while j + 1 < bytes.len() {
        if bytes[j] == b'*' && bytes[j + 1] == b'/' {
            return j + 2;
        }
        j += 1;
    }
    bytes.len()
}

fn skip_dollar_quoted(bytes: &[u8], start: usize) -> usize {
    // Non-greedy to the next $$, like the grammar's DOLLAR_QUOTED_STRING: '$$' .*? '$$'.
    let mut j = start;
    while j + 1 < bytes.len() {
        if bytes[j] == b'$' && bytes[j + 1] == b'$' {
            return j + 2;
        }
        j += 1;
    }
    bytes.len()
}

fn encode_string(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
}

/// Parenthesises a numeral that starts with `-`: spliced after a minus, `3-?`
/// bound to -5 would otherwise read `3--5` — a line comment that silently
/// drops the rest of the line.
fn signed(numeral: String) -> String {
    if numeral.starts_with('-') {
        format!("({numeral})")
    } else {
        numeral
    }
}

/// A FLOAT literal carrying the exact double: the shortest exponent form — the
/// positional one spells 1e300 in 301 digits, beyond NUMBER's range — cast to
/// FLOAT, since a numeral that fits NUMBER reads as one (`1.0` would come back
/// an integer); the non-finite values by the spellings FLOAT accepts.
fn float_literal(f: f64) -> String {
    if f.is_nan() {
        "'NaN'::FLOAT".to_string()
    } else if f.is_infinite() {
        if f > 0.0 { "'Infinity'::FLOAT" } else { "'-Infinity'::FLOAT" }.to_string()
    } else {
        signed(format!("{f:e}::FLOAT"))
    }
}

pub fn format_literal(value: &Value) -> Result<String, Error> {
    match value {
        Value::Null => Ok("NULL".to_string()),
        Value::Bool(b) => Ok(if *b { "TRUE" } else { "FALSE" }.to_string()),
        Value::Int(i) => Ok(signed(i.to_string())),
        Value::Float(f) => Ok(float_literal(*f)),
        Value::Str(s) => Ok(encode_string(s)),
        Value::Bytes(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
            Ok(format!("X'{hex}'"))
        }
        Value::Date(s) => Ok(format!("{}::DATE", encode_string(s))),
        Value::Timestamp(s) => Ok(format!("{}::TIMESTAMP_NTZ", encode_string(s))),
        Value::Array(items) => {
            let parts: Result<Vec<String>, Error> = items.iter().map(format_literal).collect();
            Ok(format!("[{}]", parts?.join(", ")))
        }
        Value::Object(_) => Err(Error::new("unsupported bind type object")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve;

    /// Serves the canned (status, body) responses, one connection each, and
    /// returns the request payloads it saw.
    fn serve_script(
        responses: Vec<(u16, &'static str)>,
    ) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        serve(
            responses
                .into_iter()
                .map(|(status, body)| {
                    format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                })
                .collect(),
        )
    }

    #[test]
    fn round_trips_learn_and_echo_the_session_id() {
        let ok = "{\"success\":true,\"sessionId\":\"s-1\",\"resultSets\":[]}";
        let (port, server) =
            serve_script(vec![(200, ok), (200, ok), (200, ok), (200, ok), (200, ok)]);
        let mut conn = Connection::new(&format!("frostlake://127.0.0.1:{port}")).unwrap();
        conn.execute("SELECT 1", &[]).unwrap();
        conn.begin().unwrap();
        conn.execute("SELECT 2", &[]).unwrap();
        conn.commit().unwrap();
        conn.execute("SELECT 3", &[]).unwrap();
        let seen = server.join().unwrap();
        // The first request carries no session yet; every later one echoes the
        // id the server issued.
        assert!(!seen[0].contains("sessionId"), "{}", seen[0]);
        assert!(seen[1].contains("\"sessionId\":\"s-1\""), "{}", seen[1]);
        assert!(seen[4].contains("\"sessionId\":\"s-1\""), "{}", seen[4]);
        // BEGIN flips autoCommit off for the whole transaction (the COMMIT
        // statement included); COMMIT restores it for what follows.
        assert!(seen[0].contains("\"autoCommit\":true"), "{}", seen[0]);
        assert!(seen[1].contains("BEGIN") && seen[1].contains("\"autoCommit\":false"), "{}", seen[1]);
        assert!(seen[2].contains("\"autoCommit\":false"), "{}", seen[2]);
        assert!(seen[3].contains("COMMIT") && seen[3].contains("\"autoCommit\":false"), "{}", seen[3]);
        assert!(seen[4].contains("\"autoCommit\":true"), "{}", seen[4]);
    }

    #[test]
    fn failed_statements_surface_the_engine_message() {
        let (port, _server) = serve_script(vec![
            (400, "{\"success\":false,\"errorMessage\":\"boom table missing\"}"),
            (400, "{\"error\":\"SQL is required\"}"),
            (200, "{\"success\":false}"),
            (500, "not json at all"),
            (200, "{\"success\":true,\"rows\":[[undefined]]}"),
        ]);
        let mut conn = Connection::new(&format!("frostlake://127.0.0.1:{port}")).unwrap();
        let engine_error = conn.execute("SELECT 1", &[]).unwrap_err();
        assert_eq!(engine_error.to_string(), "boom table missing");
        // Request validation names its reason under `error`.
        let validation = conn.execute("", &[]).unwrap_err();
        assert_eq!(validation.to_string(), "SQL is required");
        let bare_failure = conn.execute("SELECT 2", &[]).unwrap_err();
        assert_eq!(bare_failure.to_string(), "statement failed");
        let unreadable = conn.execute("SELECT 3", &[]).unwrap_err();
        assert_eq!(
            unreadable.to_string(),
            "HTTP 500 with unreadable body: invalid token at byte 0, near `not json at all`"
        );
        // A body that stops being JSON part-way is quoted around the fault.
        let broken = conn.execute("SELECT 4", &[]).unwrap_err();
        assert_eq!(
            broken.to_string(),
            "HTTP 200 with unreadable body: unexpected 'u' at byte 25, near `{\"success\":true,\"rows\":[[undefined]]}`"
        );
    }

    #[test]
    fn a_failed_begin_leaves_autocommit_on() {
        let ok = "{\"success\":true,\"sessionId\":\"s-1\",\"resultSets\":[]}";
        let (port, server) =
            serve_script(vec![(500, "{\"success\":false,\"errorMessage\":\"no\"}"), (200, ok)]);
        let mut conn = Connection::new(&format!("frostlake://127.0.0.1:{port}")).unwrap();
        assert!(conn.begin().is_err());
        conn.execute("SELECT 1", &[]).unwrap();
        let seen = server.join().unwrap();
        assert!(seen[1].contains("\"autoCommit\":true"), "{}", seen[1]);
    }

    #[test]
    fn connect_rejects_an_unhealthy_server() {
        let (port, _server) = serve_script(vec![(503, "down")]);
        let error = connect(&format!("frostlake://127.0.0.1:{port}")).unwrap_err();
        assert!(error.to_string().contains("server unhealthy: HTTP 503"), "{error}");
    }

    #[test]
    fn a_closed_connection_refuses_statements() {
        let mut conn = Connection::new("frostlake://localhost:1").unwrap();
        conn.close();
        let error = conn.execute("SELECT 1", &[]).unwrap_err();
        assert_eq!(error.to_string(), "connection is closed");
        let unreachable = connect("frostlake://127.0.0.1:1").unwrap_err();
        assert!(unreachable.to_string().contains("cannot reach 127.0.0.1:1"), "{unreachable}");
    }

    #[test]
    fn substitution_skips_literals_identifiers_and_comments() {
        let rendered = substitute(
            "SELECT 'a?b', \"c?d\", ? -- e?f\n, ? /* g?h */",
            &[Value::Str("x".into()), Value::Int(2)],
        )
        .unwrap();
        assert_eq!(rendered, "SELECT 'a?b', \"c?d\", 'x' -- e?f\n, 2 /* g?h */");
    }

    #[test]
    fn string_encoding_doubles_backslashes_then_quotes() {
        let rendered = substitute("SELECT ?", &[Value::Str("Ada O'Hara \\ Byron".into())]).unwrap();
        assert_eq!(rendered, "SELECT 'Ada O''Hara \\\\ Byron'");
    }

    #[test]
    fn typed_literal_formatting() {
        assert_eq!(format_literal(&Value::Null).unwrap(), "NULL");
        assert_eq!(format_literal(&Value::Bool(true)).unwrap(), "TRUE");
        assert_eq!(format_literal(&Value::Float(9.5)).unwrap(), "9.5e0::FLOAT");
        assert_eq!(format_literal(&Value::Bytes(vec![0xCA, 0xFE])).unwrap(), "X'CAFE'");
        assert_eq!(
            format_literal(&Value::Timestamp("2026-01-02T03:04:05".into())).unwrap(),
            "'2026-01-02T03:04:05'::TIMESTAMP_NTZ"
        );
        assert_eq!(
            format_literal(&Value::Array(vec![Value::Int(1), Value::Str("a".into())])).unwrap(),
            "[1, 'a']"
        );
    }

    #[test]
    fn json_parses_the_protocol_shapes() {
        let value = json::parse(
            "{\"success\":true,\"resultSets\":[{\"columns\":[{\"name\":\"N\"}],\"rows\":[[12345678901234567890123456789]]}]}",
        )
        .unwrap();
        assert_eq!(value.get("success"), Some(&Value::Bool(true)));
        let cell = value
            .get("resultSets")
            .and_then(Value::as_array)
            .and_then(|s| s[0].get("rows"))
            .and_then(Value::as_array)
            .and_then(|r| r[0].as_array())
            .map(|row| row[0].clone());
        assert_eq!(cell, Some(Value::Int(12345678901234567890123456789)));
    }

    #[test]
    fn json_unescapes_strings() {
        let value = json::parse("\"a\\n\\\"b\\\\c\\u00e9\\ud83d\\ude00\"").unwrap();
        assert_eq!(value, Value::Str("a\n\"b\\cé😀".to_string()));
        // A malformed pair is an error, not a panic: high surrogate followed by
        // a non-surrogate escape, and a lone high surrogate at end of string.
        assert!(json::parse("\"\\ud83d\\u0041\"").is_err());
        assert!(json::parse("\"\\ud83dx\"").is_err());
    }

    #[test]
    fn dsn_parsing_extracts_database_and_schema() {
        // Valid identifiers travel bare in either case (the engine uppercases
        // them, Snowflake-style); anything else is quoted with its exact case.
        let conn = Connection::new("frostlake://localhost:1234/My_DB?schema=public").unwrap();
        assert_eq!(conn.host, "localhost");
        assert_eq!(conn.port, 1234);
        assert_eq!(
            conn.pending_use,
            vec!["USE DATABASE My_DB".to_string(), "USE SCHEMA public".to_string()]
        );
        let quoted = Connection::new("frostlake://localhost/my-db?schema=2fast").unwrap();
        assert_eq!(quoted.port, 18082);
        assert_eq!(
            quoted.pending_use,
            vec!["USE DATABASE \"my-db\"".to_string(), "USE SCHEMA \"2fast\"".to_string()]
        );
    }

    #[test]
    fn dsn_parsing_handles_url_shapes() {
        // A query without a path.
        let no_path = Connection::new("frostlake://localhost?schema=PUBLIC").unwrap();
        assert_eq!(no_path.host, "localhost");
        assert_eq!(no_path.pending_use, vec!["USE SCHEMA PUBLIC".to_string()]);
        // http:// keeps URL semantics: default port 80, like the other drivers.
        assert_eq!(Connection::new("http://localhost/db").unwrap().port, 80);
        assert_eq!(Connection::new("frostlake://localhost").unwrap().port, 18082);
        // Bracketed IPv6 literals, with and without a port.
        let v6 = Connection::new("frostlake://[::1]:9999/db").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 9999));
        assert_eq!(Connection::new("frostlake://[::1]/db").unwrap().port, 18082);
        // %-escapes decode in path and query; '+' means space only in the query.
        let decoded = Connection::new("frostlake://h/my%20db?schema=a+b%3F").unwrap();
        assert_eq!(
            decoded.pending_use,
            vec!["USE DATABASE \"my db\"".to_string(), "USE SCHEMA \"a b?\"".to_string()]
        );
        let plus_in_path = Connection::new("frostlake://h/a+b").unwrap();
        assert_eq!(plus_in_path.pending_use, vec!["USE DATABASE \"a+b\"".to_string()]);
        let unicode = Connection::new("frostlake://h/caf%C3%A9").unwrap();
        assert_eq!(unicode.pending_use, vec!["USE DATABASE \"café\"".to_string()]);
        assert!(Connection::new("frostlake://h/bad%zz").is_err());
        assert!(Connection::new("ftp://h/db").is_err());
        assert!(Connection::new("frostlake://[::1/db").is_err());
        assert!(Connection::new("frostlake:///db").is_err());
        assert!(Connection::new("frostlake://h:not_a_port/db").is_err());
        assert!(Connection::new("localhost:18082").is_err());
    }

    #[test]
    fn typed_literal_formatting_edges() {
        assert_eq!(format_literal(&Value::Int(-7)).unwrap(), "(-7)");
        assert_eq!(format_literal(&Value::Date("2026-08-19".into())).unwrap(), "'2026-08-19'::DATE");
        assert_eq!(
            format_literal(&Value::Array(vec![Value::Array(vec![Value::Null])])).unwrap(),
            "[[NULL]]"
        );
        assert_eq!(format_literal(&Value::Bytes(Vec::new())).unwrap(), "X''");
        assert_eq!(format_literal(&Value::Float(f64::NAN)).unwrap(), "'NaN'::FLOAT");
        assert_eq!(format_literal(&Value::Float(f64::INFINITY)).unwrap(), "'Infinity'::FLOAT");
        assert_eq!(format_literal(&Value::Float(f64::NEG_INFINITY)).unwrap(), "'-Infinity'::FLOAT");
        assert!(format_literal(&Value::Object(Vec::new())).is_err());
    }

    #[test]
    fn substitution_skips_slash_slash_comments() {
        let rendered = substitute("SELECT ? // c?d\n, ?", &[Value::Int(1), Value::Int(2)]).unwrap();
        assert_eq!(rendered, "SELECT 1 // c?d\n, 2");
    }

    #[test]
    fn quoted_identifiers_double_embedded_quotes() {
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident("_ok$2"), "_ok$2");
        assert_eq!(quote_ident("$lead"), "\"$lead\"");
        assert_eq!(quote_ident(""), "\"\"");
    }

    #[test]
    fn binary_conversion_requires_clean_hex() {
        let binary = Some("BINARY".to_string());
        assert_eq!(convert(Value::Str("CAFE".into()), &binary), Value::Bytes(vec![0xCA, 0xFE]));
        assert_eq!(convert(Value::Str(String::new()), &binary), Value::Bytes(Vec::new()));
        assert_eq!(convert(Value::Str("CAF".into()), &binary), Value::Str("CAF".into()));
        assert_eq!(convert(Value::Str("ZZ".into()), &binary), Value::Str("ZZ".into()));
        assert_eq!(convert(Value::Null, &binary), Value::Null);
        assert_eq!(
            convert(Value::Str("x".into()), &Some("TIMESTAMP_LTZ".to_string())),
            Value::Timestamp("x".into())
        );
        assert_eq!(convert(Value::Str("t".into()), &None), Value::Str("t".into()));
    }

    #[test]
    fn column_lookup_prefers_exact_then_falls_back_case_insensitively() {
        let ambiguous = QueryResult {
            columns: vec![
                Column { name: "n".to_string(), data_type: None },
                Column { name: "N".to_string(), data_type: None },
            ],
            rows: vec![vec![Value::Int(1), Value::Int(2)]],
            row_count: 1,
        };
        assert_eq!(ambiguous.get(0, "N"), Some(&Value::Int(2)));
        assert_eq!(ambiguous.get(0, "n"), Some(&Value::Int(1)));
        assert_eq!(ambiguous.get(0, "missing"), None);
        assert_eq!(ambiguous.get(9, "n"), None);
        let single = QueryResult {
            columns: vec![Column { name: "NAME".to_string(), data_type: None }],
            rows: vec![vec![Value::Str("Ada".into())]],
            row_count: 1,
        };
        assert_eq!(single.get(0, "name"), Some(&Value::Str("Ada".into())));
    }

    #[test]
    fn shaping_tolerates_absent_and_ragged_result_sets() {
        assert_eq!(shape_result(&json::parse("{\"success\":true}").unwrap()).row_count, 0);
        assert_eq!(shape_result(&json::parse("{\"resultSets\":[]}").unwrap()).row_count, 0);
        // A short row pads with NULLs instead of panicking.
        let ragged = json::parse(
            "{\"resultSets\":[{\"columns\":[{\"name\":\"A\"},{\"name\":\"B\"}],\"rows\":[[1]]}]}",
        )
        .unwrap();
        let shaped = shape_result(&ragged);
        assert_eq!(shaped.rows, vec![vec![Value::Int(1), Value::Null]]);
        assert_eq!(shaped.row_count, 1);
    }

    #[test]
    fn substitution_skips_dollar_quoted_strings() {
        let rendered = substitute("SELECT $$a?b$$ AS s, ? AS n", &[Value::Int(7)]).unwrap();
        assert_eq!(rendered, "SELECT $$a?b$$ AS s, 7 AS n");
        // An unterminated $$ swallows the rest, like the grammar's non-greedy lexer rule.
        assert_eq!(substitute("SELECT $$a?b", &[]).unwrap(), "SELECT $$a?b");
    }

    #[test]
    fn substitution_rejects_bind_count_mismatch() {
        let not_enough = substitute("SELECT ?, ?", &[Value::Int(1)]).unwrap_err();
        assert!(not_enough.to_string().contains("not enough bind values"), "{not_enough}");
        let too_many = substitute("SELECT ?", &[Value::Int(1), Value::Int(2)]).unwrap_err();
        assert_eq!(too_many.to_string(), "too many bind values: 2 given, 1 placeholder");
    }

    #[test]
    fn dml_counts_sum_like_the_jdbc_driver() {
        // UPDATE: two columns, the always-0 multi-joined one excluded from the count.
        let update = json::parse(
            "{\"resultSets\":[{\"columns\":[{\"name\":\"number of rows updated\"},{\"name\":\"number of multi-joined rows updated\"}],\"rows\":[[5,0]]}]}",
        )
        .unwrap();
        let shaped = shape_result(&update);
        assert_eq!(shaped.row_count, 5);
        // The count row stays readable as the server answered it.
        assert_eq!(shaped.columns.len(), 2);
        assert_eq!(shaped.rows, vec![vec![Value::Int(5), Value::Int(0)]]);

        // MERGE: one column per action, summed.
        let merge = json::parse(
            "{\"resultSets\":[{\"columns\":[{\"name\":\"number of rows inserted\"},{\"name\":\"number of rows updated\"}],\"rows\":[[2,3]]}]}",
        )
        .unwrap();
        assert_eq!(shape_result(&merge).row_count, 5);

        // An aliased look-alike stays an ordinary one-row query result.
        let alias = json::parse(
            "{\"resultSets\":[{\"columns\":[{\"name\":\"number of rows once\"}],\"rows\":[[7]]}]}",
        )
        .unwrap();
        let kept = shape_result(&alias);
        assert_eq!(kept.row_count, 1);
        assert_eq!(kept.rows, vec![vec![Value::Int(7)]]);
    }

    #[test]
    fn negative_numbers_are_parenthesised() {
        let rendered = substitute(
            "SELECT 3-?, ?-1, [?]",
            &[Value::Int(-5), Value::Float(-2.5), Value::Int(-1)],
        )
        .unwrap();
        assert_eq!(rendered, "SELECT 3-(-5), (-2.5e0::FLOAT)-1, [(-1)]");
        assert_eq!(substitute("SELECT 3-?", &[Value::Int(5)]).unwrap(), "SELECT 3-5");
    }

    #[test]
    fn float_literals_carry_the_exact_double_as_a_float() {
        for (f, literal) in [
            (1e300, "1e300::FLOAT"),
            (1e38, "1e38::FLOAT"),
            (1.0, "1e0::FLOAT"),
            (0.1, "1e-1::FLOAT"),
            (0.1 + 0.2, "3.0000000000000004e-1::FLOAT"),
            (f64::MIN_POSITIVE, "2.2250738585072014e-308::FLOAT"),
            (-0.0, "(-0e0::FLOAT)"),
        ] {
            assert_eq!(format_literal(&Value::Float(f)).unwrap(), literal);
        }
    }

    #[test]
    fn float_columns_read_non_finite_text() {
        let float = Some("FLOAT".to_string());
        let double = Some("DOUBLE".to_string());
        assert!(matches!(convert(Value::Str("NaN".into()), &float), Value::Float(f) if f.is_nan()));
        assert_eq!(convert(Value::Str("Infinity".into()), &float), Value::Float(f64::INFINITY));
        assert_eq!(
            convert(Value::Str("-Infinity".into()), &double),
            Value::Float(f64::NEG_INFINITY)
        );
        assert_eq!(convert(Value::Float(2.5), &double), Value::Float(2.5));
        assert_eq!(convert(Value::Str("n/a".into()), &float), Value::Str("n/a".into()));
    }
}
