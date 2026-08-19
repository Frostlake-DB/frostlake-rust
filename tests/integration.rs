//! Boots a real DatabaseHttpServer from FROSTLAKE_CLASSPATH and drives the
//! whole surface over it; skips itself when the variable is unset. One test
//! function so the server child lives exactly as long as the scenarios and is
//! killed by the guard's Drop even on panic.

use frostlake::{connect, Value};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn full_driver_flow() {
    let Ok(classpath) = std::env::var("FROSTLAKE_CLASSPATH") else {
        eprintln!("skipping integration test: FROSTLAKE_CLASSPATH not set");
        return;
    };
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
    let _guard = ServerGuard(child);

    let dsn = format!("frostlake://127.0.0.1:{port}");
    let mut conn = None;
    for _ in 0..100 {
        match connect(&dsn) {
            Ok(c) => {
                conn = Some(c);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    let mut conn = conn.expect("server did not become healthy");

    // DDL, DML and a typed query through binds.
    conn.execute("CREATE OR REPLACE DATABASE rs_test_db", &[]).unwrap();
    conn.execute("USE DATABASE rs_test_db", &[]).unwrap();
    conn.execute(
        "CREATE TABLE people (id INTEGER, name VARCHAR, score FLOAT, ok BOOLEAN)",
        &[],
    )
    .unwrap();
    let inserted = conn
        .execute(
            "INSERT INTO people VALUES (?, ?, ?, ?), (?, ?, ?, ?)",
            &[
                Value::Int(1),
                Value::Str("Ada O'Hara \\ Byron".into()),
                Value::Float(9.5),
                Value::Bool(true),
                Value::Int(2),
                Value::Str("Grace".into()),
                Value::Float(8.25),
                Value::Bool(false),
            ],
        )
        .unwrap();
    assert_eq!(inserted.row_count, 2);
    let result = conn
        .execute("SELECT id, name, score, ok FROM people WHERE id = ?", &[Value::Int(1)])
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.get(0, "ID"), Some(&Value::Int(1)));
    assert_eq!(result.get(0, "NAME"), Some(&Value::Str("Ada O'Hara \\ Byron".into())));
    assert_eq!(result.get(0, "SCORE"), Some(&Value::Float(9.5)));
    assert_eq!(result.get(0, "OK"), Some(&Value::Bool(true)));

    // Session state persists across statements (the table is unqualified).
    let seen = conn.execute("SELECT COUNT(*) AS n FROM people", &[]).unwrap();
    assert_eq!(seen.get(0, "N"), Some(&Value::Int(2)));

    // Transaction rollback.
    conn.execute("CREATE TABLE acc (n INTEGER)", &[]).unwrap();
    conn.execute("INSERT INTO acc VALUES (1)", &[]).unwrap();
    conn.begin().unwrap();
    conn.execute("INSERT INTO acc VALUES (2)", &[]).unwrap();
    conn.rollback().unwrap();
    let count = conn.execute("SELECT COUNT(*) AS n FROM acc", &[]).unwrap();
    assert_eq!(count.get(0, "N"), Some(&Value::Int(1)));

    // Integral NUMBER stays exact far past i64.
    let big = conn
        .execute("SELECT 12345678901234567890123456789::NUMBER(38,0) AS n", &[])
        .unwrap();
    assert_eq!(big.get(0, "N"), Some(&Value::Int(12345678901234567890123456789)));

    // Temporal and binary binds round-trip with typed results.
    conn.execute(
        "CREATE TABLE stamps (id INTEGER, moment TIMESTAMP_NTZ, d DATE, b BINARY)",
        &[],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO stamps VALUES (?, ?, ?, ?)",
        &[
            Value::Int(1),
            Value::Timestamp("2026-08-13T12:34:56.789000".into()),
            Value::Date("2026-08-13".into()),
            Value::Bytes(vec![0xCA, 0xFE]),
        ],
    )
    .unwrap();
    let row = conn.execute("SELECT moment, d, b FROM stamps WHERE id = 1", &[]).unwrap();
    match row.get(0, "MOMENT") {
        Some(Value::Timestamp(t)) => assert!(t.starts_with("2026-08-13 12:34:56.789"), "MOMENT = {t}"),
        other => panic!("MOMENT = {other:?}"),
    }
    match row.get(0, "D") {
        Some(Value::Date(d)) => assert_eq!(d, "2026-08-13"),
        other => panic!("D = {other:?}"),
    }
    match row.get(0, "B") {
        Some(Value::Bytes(b)) => assert_eq!(b, &vec![0xCAu8, 0xFE]),
        other => panic!("B = {other:?}"),
    }

    // NULL binds round-trip to NULL cells; reads are case-insensitive; a
    // SELECT's row_count is its row total.
    conn.execute("INSERT INTO people VALUES (?, ?, ?, ?)",
        &[Value::Int(3), Value::Null, Value::Null, Value::Null]).unwrap();
    let nulls = conn.execute("SELECT name, score FROM people WHERE id = 3", &[]).unwrap();
    assert_eq!(nulls.get(0, "NAME"), Some(&Value::Null));
    assert_eq!(nulls.get(0, "score"), Some(&Value::Null));
    assert_eq!(nulls.row_count, 1);
    conn.execute("DELETE FROM people WHERE id = 3", &[]).unwrap();

    // Transaction commit persists.
    conn.begin().unwrap();
    conn.execute("INSERT INTO acc VALUES (3)", &[]).unwrap();
    conn.commit().unwrap();
    let committed = conn.execute("SELECT COUNT(*) AS n FROM acc", &[]).unwrap();
    assert_eq!(committed.get(0, "N"), Some(&Value::Int(2)));

    // Every timestamp flavour converts to Value::Timestamp; TIME stays text;
    // VARBINARY decodes like BINARY.
    conn.execute(
        "CREATE TABLE flavours (ltz TIMESTAMP_LTZ, tz TIMESTAMP_TZ, t TIME, vb VARBINARY)",
        &[],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO flavours VALUES ('2026-08-19 10:00:00', '2026-08-19 10:00:00 +02:00', '12:34:56', X'CAFE')",
        &[],
    )
    .unwrap();
    let flavours = conn.execute("SELECT ltz, tz, t, vb FROM flavours", &[]).unwrap();
    match flavours.get(0, "LTZ") {
        Some(Value::Timestamp(v)) => assert!(v.starts_with("2026-08-19 10:00:00"), "LTZ = {v}"),
        other => panic!("LTZ = {other:?}"),
    }
    match flavours.get(0, "TZ") {
        Some(Value::Timestamp(v)) => assert!(v.starts_with("2026-08-19 10:00:00"), "TZ = {v}"),
        other => panic!("TZ = {other:?}"),
    }
    match flavours.get(0, "T") {
        Some(Value::Str(v)) => assert!(v.starts_with("12:34:56"), "T = {v}"),
        other => panic!("T = {other:?}"),
    }
    match flavours.get(0, "VB") {
        Some(Value::Bytes(b)) => assert_eq!(b, &vec![0xCAu8, 0xFE]),
        other => panic!("VB = {other:?}"),
    }

    // An Array bind renders as an array literal — usable in expressions (the
    // engine deliberately rejects them inside INSERT ... VALUES, like Snowflake).
    let arr = conn
        .execute("SELECT ? AS a", &[Value::Array(vec![Value::Int(1), Value::Str("x".into())])])
        .unwrap();
    match arr.get(0, "A") {
        Some(Value::Str(text)) => assert!(text.contains('1') && text.contains('x'), "A = {text}"),
        other => panic!("A = {other:?}"),
    }

    // A multi-statement request answers with several result sets; the driver
    // surfaces the first, as documented.
    let multi = conn.execute("SELECT 1 AS one; SELECT 2 AS two", &[]).unwrap();
    assert_eq!(multi.get(0, "ONE"), Some(&Value::Int(1)));

    // Dollar-quoted strings pass through substitution untouched.
    let dollar = conn.execute("SELECT $$a?b$$ AS s, ? AS n", &[Value::Int(7)]).unwrap();
    assert_eq!(dollar.get(0, "S"), Some(&Value::Str("a?b".into())));
    assert_eq!(dollar.get(0, "N"), Some(&Value::Int(7)));

    // MERGE, UPDATE and DELETE report affected-row counts: UPDATE's always-0
    // "multi-joined" column stays out of the sum, MERGE's per-action counts add up.
    conn.execute("CREATE TABLE tgt (id INTEGER, v VARCHAR)", &[]).unwrap();
    conn.execute("CREATE TABLE src (id INTEGER, v VARCHAR)", &[]).unwrap();
    conn.execute("INSERT INTO tgt VALUES (1, 'old')", &[]).unwrap();
    conn.execute("INSERT INTO src VALUES (1, 'new'), (2, 'ins')", &[]).unwrap();
    let merged = conn
        .execute(
            "MERGE INTO tgt USING src ON tgt.id = src.id \
             WHEN MATCHED THEN UPDATE SET v = src.v \
             WHEN NOT MATCHED THEN INSERT VALUES (src.id, src.v)",
            &[],
        )
        .unwrap();
    assert_eq!(merged.row_count, 2);
    // The per-action count row stays readable, as the server answered it.
    assert_eq!(merged.rows, vec![vec![Value::Int(1), Value::Int(1)]]);
    let updated = conn.execute("UPDATE tgt SET v = 'x' WHERE id IN (1, 2)", &[]).unwrap();
    assert_eq!(updated.row_count, 2);
    assert_eq!(updated.rows, vec![vec![Value::Int(2), Value::Int(0)]]);
    let deleted = conn.execute("DELETE FROM tgt WHERE id IN (1, 2)", &[]).unwrap();
    assert_eq!(deleted.row_count, 2);

    // A DSN naming database and schema lands the session there (identifiers
    // travel bare, so the engine uppercases them Snowflake-style).
    let mut scoped = connect(&format!("frostlake://127.0.0.1:{port}/rs_test_db?schema=public"))
        .expect("DSN with database/schema");
    let seen = scoped.execute("SELECT COUNT(*) AS n FROM people", &[]).unwrap();
    assert_eq!(seen.get(0, "N"), Some(&Value::Int(2)));

    // A DSN naming a database that does not exist fails on first use — and
    // keeps failing rather than silently running in the default database.
    let mut broken = connect(&format!("frostlake://127.0.0.1:{port}/rs_no_such_db")).unwrap();
    assert!(broken.execute("SELECT 1", &[]).is_err());
    assert!(broken.execute("SELECT 1", &[]).is_err());

    // A negative bind after a minus stays a number, not a `--` comment that
    // would swallow the rest of the line (the alias included).
    let diff = conn.execute("SELECT 3-? AS r", &[Value::Int(-5)]).unwrap();
    assert_eq!(diff.get(0, "R"), Some(&Value::Int(8)));
    let scaled = conn.execute("SELECT 10-? AS r", &[Value::Float(-2.5)]).unwrap();
    assert_eq!(scaled.get(0, "R"), Some(&Value::Float(12.5)));

    // Float binds keep the exact double and the FLOAT type at every
    // magnitude, and NaN and the infinities round-trip through a FLOAT column.
    conn.execute("CREATE TABLE doubles (id INTEGER, f FLOAT)", &[]).unwrap();
    let samples = [
        1e300,
        1e38,
        f64::MIN_POSITIVE,
        -0.1,
        0.1 + 0.2,
        1.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    for (id, f) in samples.iter().enumerate() {
        conn.execute("INSERT INTO doubles VALUES (?, ?)", &[Value::Int(id as i128), Value::Float(*f)])
            .unwrap();
    }
    conn.execute("INSERT INTO doubles VALUES (?, ?)", &[Value::Int(98), Value::Float(-0.0)]).unwrap();
    conn.execute("INSERT INTO doubles VALUES (?, ?)", &[Value::Int(99), Value::Float(f64::NAN)]).unwrap();
    let doubles = conn.execute("SELECT f FROM doubles ORDER BY id", &[]).unwrap();
    for (row, f) in samples.iter().enumerate() {
        assert_eq!(doubles.rows[row][0], Value::Float(*f), "row {row}");
    }
    // -0.0 keeps its sign wherever the engine itself keeps one — 0.0.7's FLOAT
    // drops it even for a bare -0.0e0::FLOAT expression.
    let bare = conn.execute("SELECT -0.0e0::FLOAT AS z", &[]).unwrap();
    let engine_keeps_sign =
        matches!(bare.get(0, "Z"), Some(Value::Float(z)) if z.is_sign_negative());
    match &doubles.rows[samples.len()][0] {
        Value::Float(f) if *f == 0.0 && f.is_sign_negative() == engine_keeps_sign => {}
        other => panic!("negative zero row = {other:?} (engine keeps the sign: {engine_keeps_sign})"),
    }
    match &doubles.rows[samples.len() + 1][0] {
        Value::Float(f) if f.is_nan() => {}
        other => panic!("NaN row = {other:?}"),
    }
    let one = conn.execute("SELECT ? AS f", &[Value::Float(1.0)]).unwrap();
    assert_eq!(one.get(0, "F"), Some(&Value::Float(1.0)));
    assert_eq!(one.columns[0].data_type.as_deref(), Some("FLOAT"));

    // A blank statement is refused by request validation, whose answer names
    // the reason under `error` rather than `errorMessage`.
    let blank = conn.execute("   ", &[]).unwrap_err();
    assert_eq!(blank.to_string(), "SQL is required");

    // The error surface carries the engine's message.
    let error = conn.execute("SELECT FROM nowhere", &[]).unwrap_err();
    assert!(error.to_string().contains("SQL compilation error"), "{error}");
}
