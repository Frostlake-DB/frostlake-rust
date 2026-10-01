//! Runs the engine-owned, language-neutral JSON test suites through THIS driver.
//!
//! The definitions live in the frostlake repo
//! (`engine/src/test/resources/testkit/suites/*.json`, spec in `SCHEMA.md` beside them);
//! every statement travels this driver -> HTTP -> `DatabaseHttpServer`. The engine owns the
//! definitions and this file is only the Rust runner — a port of the Java reference
//! runner's `Runner`/`Compare` — so suites added on the engine side are picked up here with
//! no driver change.
//!
//! ```sh
//! export FL_CORPUS=/path/to/frostlake/engine/src/test/resources/testkit
//! FROSTLAKE_CLASSPATH="<engine jar + deps>" cargo test --test suites -- --nocapture  # boots a fresh engine
//! FROSTLAKE_URL=frostlake://localhost:18082 cargo test --test suites -- --nocapture  # or attach to one
//! ```
//!
//! `FL_CORPUS` names the testkit directory whose `suites/*.json` run: without it `build.rs`
//! leaves this test ignored, and a directory holding no suites fails it. With no engine named
//! the whole thing skips, so a checkout without one is never falsely green. The per-test
//! report lands in `target/tmp/testkit-rust.tsv`.
//!
//! Semantics (mirroring SCHEMA.md and the other drivers' runners):
//!   - backend name for a suite's skip clause: `rust`; `http` entries are honoured too,
//!     since this driver rides the HTTP transport.
//!   - per-test isolation: `CREATE OR REPLACE DATABASE test_db` -> `USE` ->
//!     `CREATE OR REPLACE SCHEMA test_schema` -> `USE`, then the steps on ONE connection,
//!     which is what keeps `USE`, variables and transactions on a single session.
//!   - capabilities: SESSION, COLUMN_NAMES, UPDATE_COUNT (derived from the count grid, as the
//!     reference does). No ERROR_CODE — the protocol carries a message only, so an expected
//!     error's `code`/`sqlState` is counted as a missing API rather than as a failure.
//!   - values compare after the reference's normalisation: NULL and booleans folded, anything
//!     numeric rounded to 10 significant digits, everything else trimmed text.

#[allow(dead_code)]
#[path = "../src/json.rs"]
mod json;

use frostlake::{connect, Column, Connection, QueryResult, Value};
use json::Value as J;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const RESET: [&str; 4] = [
    "CREATE OR REPLACE DATABASE test_db",
    "USE DATABASE test_db",
    "CREATE OR REPLACE SCHEMA test_schema",
    "USE SCHEMA test_schema",
];

const MAX_REPORTED_FAILURES: usize = 20;

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[cfg_attr(
    not(fl_corpus),
    ignore = "set FL_CORPUS to frostlake's engine/src/test/resources/testkit to replay the testkit corpus"
)]
fn testkit_suites_run_through_this_driver() {
    let files = suite_files();
    let Some((dsn, _server)) = engine() else {
        eprintln!("skipping testkit: set FROSTLAKE_URL, or FROSTLAKE_CLASSPATH to boot an engine");
        return;
    };

    let mut conn = None;
    for _ in 0..150 {
        match connect(&dsn) {
            Ok(c) => {
                conn = Some(c);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    let mut conn = conn.expect("the engine did not become healthy");
    // Cases whose step holds several statements are packs, which the engine refuses unless the
    // session asks for them; the corpus asks once, for any number.
    let _ = conn.execute("ALTER SESSION SET MULTI_STATEMENT_COUNT = 0", &[]);

    let started = Instant::now();
    let mut tsv = String::from("suite\ttest\tstatus\tfailedStep\tdetail\tms\n");
    let (mut passed, mut failed, mut skipped, mut missing_api) = (0usize, 0usize, 0usize, 0usize);
    let mut first_failures = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read suite");
        let suite = json::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let suite_name = str_field(&suite, "suite")
            .map(str::to_string)
            .unwrap_or_else(|| file.file_stem().unwrap().to_string_lossy().into_owned());
        let Some(J::Array(tests)) = suite.get("tests") else { continue };
        for test in tests {
            let name = str_field(test, "name").unwrap_or("").to_string();
            let t0 = Instant::now();
            let (status, step, detail) = match skip_reason(test) {
                Some(reason) => ("SKIP", None, reason),
                None => run_test(&mut conn, test, &mut missing_api),
            };
            match status {
                "PASS" => passed += 1,
                "SKIP" => skipped += 1,
                _ => {
                    failed += 1;
                    if first_failures.len() < MAX_REPORTED_FAILURES {
                        first_failures.push(format!("{suite_name} / {name}: {status} {detail}"));
                    }
                }
            }
            let ms = if status == "SKIP" { 0 } else { t0.elapsed().as_millis() };
            let step = step.map(|s: usize| s.to_string()).unwrap_or_default();
            let detail = detail.replace('\t', " ").replace('\n', " ");
            tsv.push_str(&format!("{suite_name}\t{name}\t{status}\t{step}\t{detail}\t{ms}\n"));
        }
    }
    let report = Path::new(env!("CARGO_TARGET_TMPDIR")).join("testkit-rust.tsv");
    std::fs::write(&report, tsv).expect("write report");

    eprintln!(
        "\ntestkit [rust]: {passed} passed, {failed} failed, {skipped} skipped, {missing_api} check(s) \
         needing an API the HTTP transport lacks — {} suite files in {:.1} s, report {}",
        files.len(),
        started.elapsed().as_secs_f64(),
        report.display()
    );
    for line in &first_failures {
        eprintln!("  {}", line.chars().take(300).collect::<String>());
    }
    assert_eq!(failed, 0, "testkit cases failed; see {}", report.display());
}

/// The DSN to run against, and the server this test booted for it, if it booted one.
fn engine() -> Option<(String, Option<ServerGuard>)> {
    if let Ok(url) = std::env::var("FROSTLAKE_URL") {
        return Some((url, None));
    }
    let classpath = std::env::var("FROSTLAKE_CLASSPATH").ok()?;
    let java = match std::env::var("JAVA_HOME") {
        Ok(home) => format!("{home}/bin/java"),
        Err(_) => "java".to_string(),
    };
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let child = Command::new(&java)
        .arg("-cp")
        .arg(&classpath)
        .arg("dev.frostlake.http.DatabaseHttpServer")
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the Frostlake server");
    Some((format!("frostlake://127.0.0.1:{port}"), Some(ServerGuard(child))))
}

/// The suite definitions: `suites/*.json` under the testkit directory `FL_CORPUS` names, in
/// name order. A directory holding none is a wrong `FL_CORPUS`, never an empty pass.
fn suite_files() -> Vec<PathBuf> {
    let corpus = PathBuf::from(std::env::var_os("FL_CORPUS").unwrap_or_default());
    let mut files: Vec<PathBuf> = std::fs::read_dir(corpus.join("suites"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    assert!(
        !files.is_empty(),
        "FL_CORPUS={}: no suites/*.json there; point it at frostlake's engine/src/test/resources/testkit",
        corpus.display()
    );
    files
}

fn str_field<'a>(v: &'a J, key: &str) -> Option<&'a str> {
    match v.get(key) {
        Some(J::Str(s)) => Some(s),
        _ => None,
    }
}

/// The skip clause names this runner: `rust`, or `http` — the transport it speaks.
fn skip_reason(test: &J) -> Option<String> {
    let skip = test.get("skip")?;
    let Some(J::Array(backends)) = skip.get("backends") else { return None };
    for backend in backends {
        if let J::Str(name) = backend {
            if name.eq_ignore_ascii_case("rust") || name.eq_ignore_ascii_case("http") {
                return Some(
                    str_field(skip, "reason")
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("skipped for {name}")),
                );
            }
        }
    }
    None
}

fn run_test(conn: &mut Connection, test: &J, missing_api: &mut usize) -> (&'static str, Option<usize>, String) {
    for sql in RESET {
        if let Err(e) = conn.execute(sql, &[]) {
            return ("ERROR", None, format!("resetContext failed on '{sql}': {e}"));
        }
    }
    if let Some(J::Array(steps)) = test.get("steps") {
        for (i, step) in steps.iter().enumerate() {
            let sql = str_field(step, "sql").unwrap_or("");
            let outcome = conn.execute(sql, &[]);
            if let Err(e) = &outcome {
                // A transport failure is an ERROR, as in the reference — never an "expected error".
                let text = e.to_string();
                if text.starts_with("request failed:") || text.contains("with unreadable body") {
                    return ("ERROR", Some(i + 1), format!("{text}  [sql: {sql}]"));
                }
            }
            if let Err(detail) = check(step.get("expect"), &outcome, missing_api) {
                return ("FAIL", Some(i + 1), format!("{detail}  [sql: {sql}]"));
            }
        }
    }
    ("PASS", None, String::new())
}

fn check(
    expect: Option<&J>,
    outcome: &Result<QueryResult, frostlake::Error>,
    missing_api: &mut usize,
) -> Result<(), String> {
    let expect = expect.filter(|e| matches!(e, J::Object(_)));
    if let Some(error) = expect.and_then(|e| e.get("error")).filter(|e| matches!(e, J::Object(_))) {
        let Err(e) = outcome else { return Err("expected an error, statement succeeded".to_string()) };
        let message = e.to_string();
        if let Some(want) = str_field(error, "messageContains") {
            if !message.to_lowercase().contains(&want.to_lowercase()) {
                return Err(format!("error message [{message}] does not contain [{want}]"));
            }
        }
        if str_field(error, "code").is_some() || str_field(error, "sqlState").is_some() {
            *missing_api += 1;
        }
        return Ok(());
    }
    let result = match outcome {
        Ok(r) => r,
        Err(e) => return Err(format!("unexpected error: {e}")),
    };
    let Some(expect) = expect else { return Ok(()) };
    if let Some(want) = expect.get("value") {
        let want = expected_cell(want);
        let actual = result
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(|cell| column_text(cell, result.columns.first()));
        if norm(want.as_deref()) != norm(actual.as_deref()) {
            return Err(format!(
                "value [{}] != expected [{}]",
                actual.as_deref().unwrap_or("null"),
                want.as_deref().unwrap_or("null")
            ));
        }
    }
    if let Some(J::Array(rows)) = expect.get("rows") {
        let want: Vec<Vec<Option<String>>> = rows
            .iter()
            .map(|row| match row {
                J::Array(cells) => cells.iter().map(expected_cell).collect(),
                _ => Vec::new(),
            })
            .collect();
        let got: Vec<Vec<Option<String>>> = result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(i, cell)| column_text(cell, result.columns.get(i)))
                    .collect()
            })
            .collect();
        let (mut w, mut g) = (canon(&want), canon(&got));
        if !matches!(expect.get("ordered"), Some(J::Bool(true))) {
            w.sort();
            g.sort();
        }
        if w != g {
            return Err(format!("rows differ: expected {w:?} got {g:?}"));
        }
    }
    if let Some(J::Int(n)) = expect.get("rowCount") {
        if result.rows.len() as i128 != *n {
            return Err(format!("rowCount {} != expected {n}", result.rows.len()));
        }
    }
    if let Some(J::Array(columns)) = expect.get("columns") {
        if result.columns.len() != columns.len() {
            let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
            return Err(format!("column count {} != expected {} {names:?}", names.len(), columns.len()));
        }
        for (i, (want, got)) in columns.iter().zip(&result.columns).enumerate() {
            let want = expected_cell(want).unwrap_or_default();
            if want.to_lowercase() != got.name.to_lowercase() {
                return Err(format!("column[{i}] [{}] != expected [{want}]", got.name));
            }
        }
    }
    if let Some(J::Int(n)) = expect.get("updateCount") {
        // Derived from the count grid, as the reference does: one row, every column a "number of …".
        let grid = result.rows.len() == 1
            && !result.columns.is_empty()
            && result.columns.iter().all(|c| c.name.to_lowercase().starts_with("number of"));
        let got = if grid { result.row_count as i128 } else { -1 };
        if got != *n {
            return Err(format!("updateCount {got} != expected {n}"));
        }
    }
    Ok(())
}

fn canon(grid: &[Vec<Option<String>>]) -> Vec<String> {
    grid.iter()
        .map(|row| row.iter().map(|c| format!("{}\u{1f}", norm(c.as_deref()))).collect())
        .collect()
}

/// An expected cell as text, the way the reference reads it (null stays SQL NULL).
fn expected_cell(v: &J) -> Option<String> {
    match v {
        J::Null => None,
        J::Str(s) => Some(s.clone()),
        J::Bool(b) => Some(b.to_string()),
        J::Int(i) => Some(i.to_string()),
        J::Float(f) => Some(format!("{f:e}")),
        other => Some(java_text(other)),
    }
}

/// A driver cell as text, decoded once when its column carries semi-structured values.
fn column_text(v: &Value, column: Option<&Column>) -> Option<String> {
    let text = cell_text(v)?;
    let semi = column.is_some_and(|c| semi_structured(c.data_type.as_deref()));
    Some(if semi { semi_structured_value(text) } else { text })
}

/// Whether a column carries semi-structured values, read from the type the server declared.
fn semi_structured(data_type: Option<&str>) -> bool {
    matches!(
        data_type.unwrap_or_default().to_ascii_uppercase().as_str(),
        "VARIANT" | "OBJECT" | "ARRAY"
    )
}

/// The value a semi-structured cell carries, as the suites record it.
///
/// A VARIANT, OBJECT or ARRAY cell reaches a client as its JSON TEXT — a string's own quotes
/// included — which is what the account's own drivers do. The suites record the VALUE (`a`, not
/// `"a"`), so such a cell is decoded once: a JSON string becomes its content, which for a whole
/// object or array is the object's own text, and anything else — a number, a boolean, text that
/// is not JSON at all — is left exactly as it came. Only a semi-structured COLUMN is decoded, so
/// a VARCHAR whose content merely looks quoted keeps its quotes.
fn semi_structured_value(text: String) -> String {
    match json::parse(&text) {
        Ok(J::Str(decoded)) => decoded,
        _ => text,
    }
}

/// A driver cell as text, the way the reference stringifies the wire value.
fn cell_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Int(i) => Some(i.to_string()),
        Value::Float(f) => Some(if f.is_nan() {
            "NaN".to_string()
        } else if f.is_infinite() {
            if *f > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
        } else {
            format!("{f:e}")
        }),
        Value::Str(s) | Value::Date(s) | Value::Timestamp(s) => Some(s.clone()),
        Value::Bytes(b) => Some(b.iter().map(|x| format!("{x:02X}")).collect()),
        Value::Array(items) => Some(format!(
            "[{}]",
            items.iter().map(|x| cell_text(x).unwrap_or_else(|| "null".into())).collect::<Vec<_>>().join(", ")
        )),
        Value::Object(members) => Some(format!(
            "{{{}}}",
            members
                .iter()
                .map(|(k, x)| format!("{k}={}", cell_text(x).unwrap_or_else(|| "null".into())))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// A nested expected value in Java's collection `toString` form.
fn java_text(v: &J) -> String {
    match v {
        J::Null => "null".to_string(),
        J::Array(items) => format!("[{}]", items.iter().map(java_text).collect::<Vec<_>>().join(", ")),
        J::Object(members) => format!(
            "{{{}}}",
            members.iter().map(|(k, x)| format!("{k}={}", java_text(x))).collect::<Vec<_>>().join(", ")
        ),
        other => expected_cell(other).unwrap_or_default(),
    }
}

/// The reference's `Compare.norm`: NULL and booleans folded, anything BigDecimal can read
/// rounded to 10 significant digits (HALF_UP) and printed plain, everything else trimmed text.
fn norm(v: Option<&str>) -> String {
    let Some(v) = v else { return "NULL".to_string() };
    let v = v.trim_matches(|c: char| c <= ' ');
    if v.is_empty() || v.eq_ignore_ascii_case("null") {
        return "NULL".to_string();
    }
    if v.eq_ignore_ascii_case("true") {
        return "TRUE".to_string();
    }
    if v.eq_ignore_ascii_case("false") {
        return "FALSE".to_string();
    }
    decimal_norm(v).unwrap_or_else(|| v.to_string())
}

fn decimal_norm(v: &str) -> Option<String> {
    let b = v.as_bytes();
    let mut i = 0;
    let negative = match b.first() {
        Some(b'-') => {
            i = 1;
            true
        }
        Some(b'+') => {
            i = 1;
            false
        }
        _ => false,
    };
    let mut digits: Vec<u8> = Vec::new();
    let mut frac_len: i64 = 0;
    let mut seen_point = false;
    while i < b.len() {
        match b[i] {
            c @ b'0'..=b'9' => {
                digits.push(c - b'0');
                if seen_point {
                    frac_len += 1;
                }
            }
            b'.' if !seen_point => seen_point = true,
            _ => break,
        }
        i += 1;
    }
    if digits.is_empty() {
        return None;
    }
    let mut exponent: i64 = 0;
    if i < b.len() {
        if b[i] != b'e' && b[i] != b'E' {
            return None;
        }
        exponent = v[i + 1..].parse::<i64>().ok()?;
        if exponent.abs() > 100_000 {
            return None;
        }
    }
    let mut scale = exponent - frac_len;
    let Some(first) = digits.iter().position(|&d| d != 0) else { return Some("0".to_string()) };
    let mut sig: Vec<u8> = digits[first..].to_vec();
    if sig.len() > 10 {
        let round_up = sig[10] >= 5;
        scale += (sig.len() - 10) as i64;
        sig.truncate(10);
        if round_up {
            let mut k = sig.len();
            loop {
                if k == 0 {
                    sig.insert(0, 1);
                    sig.pop();
                    scale += 1;
                    break;
                }
                k -= 1;
                if sig[k] == 9 {
                    sig[k] = 0;
                } else {
                    sig[k] += 1;
                    break;
                }
            }
        }
    }
    while sig.len() > 1 && sig[sig.len() - 1] == 0 {
        sig.pop();
        scale += 1;
    }
    let text: String = sig.iter().map(|d| (b'0' + d) as char).collect();
    let body = if scale >= 0 {
        format!("{text}{}", "0".repeat(scale as usize))
    } else {
        let point = text.len() as i64 + scale;
        if point > 0 {
            format!("{}.{}", &text[..point as usize], &text[point as usize..])
        } else {
            format!("0.{}{text}", "0".repeat((-point) as usize))
        }
    };
    Some(if negative { format!("-{body}") } else { body })
}
