# frostlake-rust

A zero-dependency Rust driver for [Frostlake](https://frostlake.dev), speaking the
engine's HTTP protocol against a running `DatabaseHttpServer`. No crates at all —
Rust's std has neither an HTTP client nor JSON, so the driver carries its own minimal
HTTP/1.1 client over `TcpStream` (the protocol is plaintext HTTP) and a small
recursive-descent JSON parser.

## Engine version

Requires a Frostlake engine **0.0.7 or newer**. Ask a running server which one it is with
`SELECT CURRENT_VERSION()` — every release answers it, so the check works against any engine.

The driver versions independently of the engine: it speaks the HTTP protocol, not
the jar, so this is a floor rather than a lockstep pin.

## Usage

```rust
use frostlake::{connect, Value};

// A fresh engine has no MY_DB yet: create it over a connection that names none.
connect("frostlake://localhost:18082")?.execute("CREATE DATABASE IF NOT EXISTS MY_DB", &[])?;

let mut conn = connect("frostlake://localhost:18082/MY_DB?schema=PUBLIC")?;

conn.execute("CREATE OR REPLACE TABLE people (id INTEGER, name VARCHAR)", &[])?;
let inserted = conn.execute(
    "INSERT INTO people VALUES (?, ?), (?, ?)",
    &[Value::Int(1), Value::Str("Ada".into()), Value::Int(2), Value::Str("Grace".into())],
)?;
assert_eq!(inserted.row_count, 2);

let result = conn.execute("SELECT id, name FROM people WHERE id = ?", &[Value::Int(1)])?;
assert_eq!(result.get(0, "ID"), Some(&Value::Int(1)));
assert_eq!(result.get(0, "NAME"), Some(&Value::Str("Ada".into())));
```

`execute(sql, binds)` returns a `QueryResult` with `columns`, positional `rows`
(read cells by name via `result.get(row, "NAME")`) and `row_count`, the number of
rows. DML answers Snowflake-style with a single row of counts — `number of rows
inserted` for an INSERT, one column per action for a MERGE, and for an UPDATE a
companion `number of multi-joined rows updated` — which stays readable in `rows`,
while `row_count` holds the affected-row count, summed the way the engine's JDBC
driver sums it (the always-0 multi-joined column left out). A failed statement
returns `Err(frostlake::Error)` carrying the engine's error message.
`begin()`/`commit()`/`rollback()` drive transactions.

The database and schema named in the DSN are applied as `USE` statements on first
execute. Valid identifiers travel bare — the engine uppercases them, Snowflake-style
— and anything else (spaces, hyphens, a leading digit) is double-quoted with its
exact case. The DSN follows URL conventions: `%XX` escapes decode (with `+` as
space in the query), IPv6 literals are bracketed (`frostlake://[::1]:18082/db`),
and `http://` without a port means port 80 while `frostlake://` means 18082.
A multi-statement request answers with several result sets; the driver surfaces
the first.

### Bind values

Parameters are inlined client-side (`?` placeholders); placeholders inside string
literals, dollar-quoted `$$…$$` strings, quoted identifiers and comments are left
alone. Passing binds that don't match the placeholder count errs in either
direction. A negative number is parenthesised, so `3-?` bound to -5 reads
`3-(-5)` — spliced bare, `3--5` would start a line comment.

| `Value` variant | SQL literal |
| --- | --- |
| `Null` | `NULL` |
| `Bool` | `TRUE` / `FALSE` |
| `Int(i128)` | as written; `(-5)` when negative |
| `Float(f64)` | `9.5e0::FLOAT` — the shortest exponent form that round-trips the double, cast so it stays a FLOAT at any magnitude; `'NaN'::FLOAT`, `'Infinity'::FLOAT`, `'-Infinity'::FLOAT` for the non-finite values |
| `Str` | `'…'` (backslashes and quotes escaped) |
| `Bytes` | `X'hex'` |
| `Date("2026-08-13")` | `'…'::DATE` |
| `Timestamp("2026-08-13T12:34:56")` | `'…'::TIMESTAMP_NTZ` |
| `Array` | `[…]` (elements formatted recursively) |

### Result types

Integral `NUMBER` cells arrive as `Value::Int(i128)` — exact through the full
`NUMBER(38,0)` range, no floating-point degradation. Scaled `NUMBER(p,s)` cells
cross the wire as JSON decimals and land in `Value::Float(f64)` — exact only to
~15 significant digits. `FLOAT`/`DOUBLE` → `Value::Float` (NaN and ±∞ included —
they cross the wire as text),
`BOOLEAN` → `Value::Bool`, `DATE` → `Value::Date`, `TIMESTAMP*` → `Value::Timestamp`
(the wire text; pair with `chrono`/`time` in your application if you want calendar
types), `BINARY` → `Value::Bytes`; everything else stays `Value::Str`.

## Running the tests

The integration test boots a real server from the engine's compiled classes:

```sh
export JAVA_HOME=/path/to/jdk17
export FROSTLAKE_CLASSPATH="/path/to/frostlake/engine/target/classes:<engine deps>"
cargo test
```

Without `FROSTLAKE_CLASSPATH` the integration test skips itself and only the unit
tests (substitution, literal formatting, JSON, DSN parsing) run.

## Protocol

One `POST /api/execute` per statement with `{ sql, sessionId, autoCommit }`; the server
issues the `sessionId` on first contact and the driver echoes it back, so session state
(current database/schema, transactions) persists across statements. Failed statements
answer with a non-2xx status and the error JSON in the body, which the driver reads
regardless of status. `GET /api/health` backs `connect`'s reachability check. Each
round trip uses a fresh TCP connection (`Connection: close`) and sends its request
in a single segment; a connect gives up after 10 s per resolved address, and a
statement's answer is awaited for at most 300 s.
