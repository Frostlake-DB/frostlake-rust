# frostlake-rust

A zero-dependency Rust driver for [Frostlake](https://frostlake.dev), speaking the
engine's HTTP protocol against a running `DatabaseHttpServer`. No crates at all —
Rust's std has neither an HTTP client nor JSON, so the driver carries its own minimal
HTTP/1.1 client over `TcpStream` (the protocol is plaintext HTTP) and a small
recursive-descent JSON parser.

## Engine version

Requires a Frostlake engine **0.2.0 or newer**. Ask a running server which one it is with
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
rows. Each `Column` carries its `name`, its `data_type` and its `length` — the
width a text or binary column was declared with, in characters or in bytes, and
`None` for every other type. DML answers Snowflake-style with a single row of counts — `number of rows
inserted` for an INSERT, one column per action for a MERGE, and for an UPDATE a
companion `number of multi-joined rows updated` — which stays readable in `rows`,
while `row_count` holds the affected-row count, summed the way the engine's JDBC
driver sums it (the always-0 multi-joined column left out). A failed statement
returns `Err(frostlake::Error)` carrying the engine's error message; its `kind()`
is `ErrorKind::SessionLost` when the statement was not run because the engine no
longer holds the session it depended on (see [Session lifetime](#session-lifetime)),
and `ErrorKind::Other` for every other failure.
`begin()`/`commit()`/`rollback()` drive transactions.

The database and schema named in the DSN are applied as `USE` statements on first
execute. Valid identifiers travel bare — the engine uppercases them, Snowflake-style
— and anything else (spaces, hyphens, a leading digit) is double-quoted with its
exact case. The DSN follows URL conventions: `%XX` escapes decode (with `+` as
space in the query), IPv6 literals are bracketed (`frostlake://[::1]:18082/db`),
and `http://` without a port means port 80 while `frostlake://` means 18082.
A multi-statement request answers with several result sets; the driver surfaces
the first. The engine refuses a pack the caller did not ask for, as the account
does, so ask for one first: `ALTER SESSION SET MULTI_STATEMENT_COUNT = n`, or `0`
for any number.

### Statement counts per call

`execute_with_multi_statement_count(sql, binds, multi_statement_count)` declares the
count on one request instead of on the session:

```rust
let packed = conn.execute_with_multi_statement_count(
    "INSERT INTO people VALUES (3, 'Alan'); DELETE FROM people WHERE id = 1",
    &[],
    Some(2),
)?;
```

`Some(n)` says the request holds exactly `n` statements and `Some(0)` allows any
number. The count travels with that one request and outranks the session's
`MULTI_STATEMENT_COUNT` for it, but changes no session state — nothing to save and
restore, and connections shared between statements stay unaffected. `None` sends no
count at all, which is what plain `execute` does: the session's value decides, and it
starts at 1.

### Session lifetime

A connection is one engine session, and the session holds the current database
and schema, session variables, `ALTER SESSION` settings, temporary objects and an
open transaction. The engine keeps it until it is released or has sat idle for 30
minutes, and a restart loses every one. From 0.1.0 the engine says so: each answer
carries `newSession`, and a request can ask it to refuse a session it no longer
holds rather than quietly start a fresh one under the same id. The driver learns
which kind of engine it talks to from the first answer that names a session;
`conn.session_id()` is that session's id.

- **What is sent.** Once the engine has answered with `newSession`, every request
  naming the session carries `"requireSession":true`. An engine before 0.1.0 is
  sent neither that field nor the release below.
- **After a lost session.** The engine refuses the request (HTTP 404) and nothing
  ran. The driver drops the session, then:
  - if a transaction was open on it — `begin()`, or a `BEGIN` / `START TRANSACTION`
    statement — the statement fails with `ErrorKind::SessionLost`: the transaction
    is gone and the statement did not run. A transaction `begin()` opened stays
    dead until it is ended: every statement is refused with the same error without
    being sent, `commit()` reports it (nothing was committed), and `rollback()`
    succeeds without a round trip, since the engine discarded the transaction with
    the session;
  - else, if a statement on the session set up context a fresh session would not
    have — `USE`, `SET`/`UNSET`, `ALTER SESSION`, a temporary object,
    `CREATE`/`DROP` of a `DATABASE` or `SCHEMA` — the statement fails with
    `ErrorKind::SessionLost` and is not re-run, since a fresh session would run it
    somewhere else;
  - otherwise the DSN's scope (`USE DATABASE` / `USE SCHEMA`) goes onto a fresh
    session and the statement is sent once more. A second refusal fails with
    `ErrorKind::SessionLost`.

  Every statement of a request is examined, so a `USE` riding behind a leading
  `SELECT` counts too. After the error the connection stays usable: its next
  statement starts a fresh session on the DSN's scope.

  ```rust
  match conn.execute("INSERT INTO people VALUES (4, 'Edsger')", &[]) {
      Err(e) if e.kind() == frostlake::ErrorKind::SessionLost => { /* redo the unit of work */ }
      other => { other?; }
  }
  ```

  Against an engine before 0.1.0 none of this can happen: it re-creates a lost
  session under the same id at the server's default scope, and says nothing.
- **Close.** `close()` — and dropping a connection, which closes it — sends
  `DELETE /api/sessions/{id}`, which releases the session and rolls back a
  transaction it left open. It is a courtesy: bounded by five seconds, and whatever
  it meets (a 404, a 405, a closed socket, no answer) it reports nothing. A
  connection that never ran a statement, one closed a second time, and one talking
  to an engine before 0.1.0 send nothing; the last leaves its session to the
  engine's idle sweep.

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

The integration tests (`tests/integration.rs`, and `tests/session.rs`, which loses a
connection's session behind its back) boot a real server from the engine's compiled classes:

```sh
export JAVA_HOME=/path/to/jdk17
export FROSTLAKE_CLASSPATH="/path/to/frostlake/engine/target/classes:<engine deps>"
cargo test
```

Without `FROSTLAKE_CLASSPATH` the integration tests skip themselves and only the unit
tests (substitution, literal formatting, JSON, DSN parsing, and the session handling over a
scripted engine) run.

`FL_CORPUS` adds the engine's testkit corpus — the language-neutral JSON suites in the
frostlake repo — which `tests/suites.rs` replays through this driver against the same engine,
writing a per-case report to `target/tmp/testkit-rust.tsv`:

```sh
FL_CORPUS=/path/to/frostlake/engine/src/test/resources/testkit cargo test
```

Without `FL_CORPUS` that test is listed as ignored. An absolute path is safest: cargo runs the
test from the crate root, which is what a relative one resolves against.

## Protocol

One `POST /api/execute` per statement with `{ sql, sessionId, autoCommit }` — plus
`multiStatementCount` when a call declares one, and nothing at all when it does not; the server
issues the `sessionId` on first contact and the driver echoes it back, so session state
(current database/schema, transactions) persists across statements. The echo carries
`requireSession: true` once the engine is known to honour it, and closing sends
`DELETE /api/sessions/{id}` (see [Session lifetime](#session-lifetime)). Failed statements
answer with a non-2xx status and the error JSON in the body, which the driver reads
regardless of status. `GET /api/health` backs `connect`'s reachability check. Each
round trip uses a fresh TCP connection (`Connection: close`) and sends its request
in a single segment; a connect gives up after 10 s per resolved address, and a
statement's answer is awaited for at most 300 s.
