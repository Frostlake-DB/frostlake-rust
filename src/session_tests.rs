//! How a connection keeps its idea of the engine session in step with the
//! engine's: `requireSession` once the engine is known to offer it, recovery
//! from the engine's refusal of a lost session, and releasing the session on
//! close. Every request a scenario makes is one it scripted.

use crate::test_support::{Reply, ScriptedEngine};
use crate::{json, Connection, ErrorKind, Value};
use std::time::{Duration, Instant};

const SCOPE: [&str; 2] = ["USE DATABASE APP", "USE SCHEMA PUBLIC"];

/// An engine that reports `newSession` (0.1.0 and later) answering with no result set.
fn answer(session_id: &str, fresh: bool) -> Reply {
    Reply::Answer(
        200,
        format!(
            "{{\"errorMessage\":null,\"executionTimeMs\":1,\"newSession\":{fresh},\"resultSets\":[],\"sessionId\":\"{session_id}\",\"success\":true}}"
        ),
    )
}

/// The same, carrying one NUMBER column `N` holding `value`.
fn answer_number(session_id: &str, value: i64) -> Reply {
    Reply::Answer(
        200,
        format!(
            "{{\"errorMessage\":null,\"executionTimeMs\":1,\"newSession\":false,\"resultSets\":[{{\"columns\":[{{\"dataType\":\"NUMBER\",\"name\":\"N\",\"nullable\":false,\"precision\":38,\"scale\":0}}],\"rowCount\":1,\"rows\":[[{value}]],\"updateCount\":-1}}],\"sessionId\":\"{session_id}\",\"success\":true}}"
        ),
    )
}

/// An engine that predates `newSession` (0.0.7) answering.
fn legacy(session_id: &str) -> Reply {
    Reply::Answer(
        200,
        format!(
            "{{\"errorMessage\":null,\"executionTimeMs\":1,\"resultSets\":[],\"sessionId\":\"{session_id}\",\"success\":true}}"
        ),
    )
}

/// The 404 a `requireSession` request gets when its session is gone.
fn gone(session_id: &str) -> Reply {
    Reply::Answer(
        404,
        format!(
            "{{\"errorMessage\":\"Session '{session_id}' does not exist or has expired.\",\"executionTimeMs\":0,\"newSession\":false,\"resultSets\":[],\"sessionId\":null,\"success\":false}}"
        ),
    )
}

/// The engine's answer to `DELETE /api/sessions/{id}`.
fn released() -> Reply {
    Reply::Answer(
        200,
        "{\"errorMessage\":null,\"executionTimeMs\":0,\"newSession\":false,\"resultSets\":[],\"sessionId\":null,\"success\":true}"
            .to_string(),
    )
}

/// A request as the engine read it.
struct Sent {
    method: String,
    path: String,
    raw: String,
    body: Option<Value>,
}

impl Sent {
    fn field(&self, name: &str) -> Option<&Value> {
        self.body.as_ref()?.get(name)
    }

    fn sql(&self) -> Option<&str> {
        self.field("sql")?.as_str()
    }

    fn session_id(&self) -> Option<&str> {
        self.field("sessionId")?.as_str()
    }

    fn require_session(&self) -> Option<bool> {
        self.field("requireSession")?.as_bool()
    }

    fn auto_commit(&self) -> Option<bool> {
        self.field("autoCommit")?.as_bool()
    }
}

fn sent(engine: &ScriptedEngine) -> Vec<Sent> {
    engine
        .requests()
        .into_iter()
        .map(|raw| {
            let line = raw.lines().next().unwrap_or("").to_string();
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let body = raw
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .filter(|body| !body.is_empty())
                .and_then(|body| json::parse(body).ok());
            Sent { method, path, raw, body }
        })
        .collect()
}

/// The SQL of every `POST /api/execute` so far.
fn statements(engine: &ScriptedEngine) -> Vec<String> {
    sent(engine)
        .iter()
        .filter(|s| s.path == "/api/execute")
        .filter_map(|s| s.sql().map(str::to_string))
        .collect()
}

/// The path of every DELETE so far.
fn deletes(engine: &ScriptedEngine) -> Vec<String> {
    sent(engine).into_iter().filter(|s| s.method == "DELETE").map(|s| s.path).collect()
}

fn with_scope(rest: &[&str]) -> Vec<String> {
    SCOPE.iter().chain(rest).map(|s| s.to_string()).collect()
}

/// A connection on a scripted engine that reports `newSession`, holding session
/// s1 on the DSN's scope after one statement.
fn opened(engine: &ScriptedEngine) -> Connection {
    let mut conn =
        Connection::new(&format!("frostlake://127.0.0.1:{}/APP?schema=PUBLIC", engine.port)).unwrap();
    engine.reply([answer("s1", true), answer("s1", false), answer("s1", false)]);
    conn.execute("SELECT 0", &[]).unwrap();
    conn
}

#[test]
fn require_session_follows_the_first_answer() {
    let engine = ScriptedEngine::start();
    let mut conn = opened(&engine);
    engine.reply([answer("s1", false)]);
    conn.execute("SELECT 1", &[]).unwrap();
    assert_eq!(statements(&engine), with_scope(&["SELECT 0", "SELECT 1"]));
    let sent = sent(&engine);
    // The first request names no session, so there is nothing to require yet.
    assert_eq!(sent[0].session_id(), None);
    assert!(!sent[0].raw.contains("requireSession"), "{}", sent[0].raw);
    // The first answer says the engine offers it, and every request naming the
    // session carries it from then on.
    for s in &sent[1..] {
        assert_eq!(s.session_id(), Some("s1"), "{}", s.raw);
        assert_eq!(s.require_session(), Some(true), "{}", s.raw);
    }
    assert_eq!(engine.pending(), 0);
}

#[test]
fn an_older_engine_is_sent_neither_require_session_nor_a_delete() {
    let engine = ScriptedEngine::start();
    let mut conn =
        Connection::new(&format!("frostlake://127.0.0.1:{}/APP?schema=PUBLIC", engine.port)).unwrap();
    engine.reply([legacy("old1"), legacy("old1"), legacy("old1"), legacy("old1")]);
    conn.execute("SELECT 1", &[]).unwrap();
    conn.execute("SELECT 2", &[]).unwrap();
    conn.close();
    let sent = sent(&engine);
    // Its parser may refuse a field it does not know, and it has no release endpoint.
    assert_eq!(sent.len(), 4, "closing sent something to an engine with no release endpoint");
    for s in &sent {
        assert!(!s.raw.contains("requireSession"), "{}", s.raw);
    }
}

#[test]
fn a_lost_session_is_replaced_and_the_statement_sent_once_more() {
    let engine = ScriptedEngine::start();
    let mut conn = opened(&engine);
    engine.reply([gone("s1"), answer("s2", true), answer("s2", false), answer_number("s2", 2)]);
    let result = conn.execute("SELECT 2 AS N", &[]).unwrap();
    assert_eq!(result.get(0, "N"), Some(&Value::Int(2)));
    assert_eq!(
        statements(&engine),
        with_scope(&["SELECT 0", "SELECT 2 AS N", SCOPE[0], SCOPE[1], "SELECT 2 AS N"])
    );
    let sent = sent(&engine);
    // The scope goes onto a fresh session, and the statement follows it there.
    assert_eq!(sent[4].session_id(), None, "{}", sent[4].raw);
    assert_eq!(sent[6].session_id(), Some("s2"));
    assert_eq!(sent[6].require_session(), Some(true));
    assert_eq!(conn.session_id(), Some("s2"));
    assert_eq!(engine.pending(), 0);
    // The replacement is what closing releases, not the lost session.
    engine.reply([released()]);
    conn.close();
    assert_eq!(deletes(&engine), ["/api/sessions/s2"]);
}

#[test]
fn a_second_refusal_is_reported_and_the_connection_carries_on() {
    let engine = ScriptedEngine::start();
    let mut conn = opened(&engine);
    engine.reply([gone("s1"), answer("s2", true), answer("s2", false), gone("s2")]);
    let error = conn.execute("SELECT 2", &[]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SessionLost, "{error}");
    assert!(error.to_string().contains("just started"), "{error}");
    // The statement is sent once more, never twice.
    assert_eq!(statements(&engine)[3..], ["SELECT 2", SCOPE[0], SCOPE[1], "SELECT 2"]);

    engine.reply([answer("s3", true), answer("s3", false), answer("s3", false)]);
    conn.execute("SELECT 3", &[]).unwrap();
    let sent = sent(&engine);
    assert_eq!(sent[sent.len() - 3].session_id(), None, "the next statement should start a fresh session");
    assert_eq!(engine.pending(), 0);
}

#[test]
fn a_lost_transaction_is_reported_not_replaced() {
    for end in ["commit", "rollback"] {
        let engine = ScriptedEngine::start();
        let mut conn = opened(&engine);
        engine.reply([answer("s1", false), gone("s1")]);
        conn.begin().unwrap();
        let error = conn.execute("INSERT INTO T VALUES (1)", &[]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::SessionLost, "{end}: {error}");
        assert!(error.to_string().contains("transaction"), "{end}: {error}");
        // The transaction stays refused until it is ended: nothing more is sent
        // for it, and it cannot be committed.
        let before = engine.requests().len();
        let again = conn.execute("INSERT INTO T VALUES (2)", &[]).unwrap_err();
        assert_eq!(again.kind(), ErrorKind::SessionLost, "{end}: {again}");
        if end == "commit" {
            let commit = conn.commit().unwrap_err();
            assert_eq!(commit.kind(), ErrorKind::SessionLost, "{commit}");
            assert!(commit.to_string().contains("nothing was committed"), "{commit}");
        } else {
            conn.rollback().unwrap();
        }
        assert_eq!(engine.requests().len(), before, "{end}: a request went out for a transaction already gone");
        assert_eq!(statements(&engine)[3..], ["BEGIN", "INSERT INTO T VALUES (1)"]);

        // The connection stays usable: a fresh session on the DSN's scope,
        // outside any transaction.
        engine.reply([answer("s2", true), answer("s2", false), answer("s2", false)]);
        conn.execute("SELECT 1", &[]).unwrap();
        let last = sent(&engine).pop().unwrap();
        assert_eq!(last.session_id(), Some("s2"), "{end}");
        assert_eq!(last.auto_commit(), Some(true), "{end}");
        assert_eq!(engine.pending(), 0, "{end}");
    }
}

#[test]
fn a_lost_transaction_begun_in_sql_is_reported() {
    let engine = ScriptedEngine::start();
    let mut conn = opened(&engine);
    engine.reply([answer("s1", false), gone("s1")]);
    conn.execute("BEGIN TRANSACTION", &[]).unwrap();
    let error = conn.execute("INSERT INTO T VALUES (1)", &[]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SessionLost, "{error}");
    assert!(error.to_string().contains("transaction"), "{error}");
    engine.reply([answer("s2", true), answer("s2", false), answer("s2", false)]);
    conn.execute("SELECT 1", &[]).unwrap();
    assert_eq!(
        statements(&engine)[3..],
        ["BEGIN TRANSACTION", "INSERT INTO T VALUES (1)", SCOPE[0], SCOPE[1], "SELECT 1"]
    );
}

#[test]
fn a_begin_finding_a_transaction_lost_leaves_the_connection_usable() {
    let engine = ScriptedEngine::start();
    let mut conn = opened(&engine);
    engine.reply([answer("s1", false), gone("s1")]);
    conn.execute("BEGIN", &[]).unwrap();
    // The BEGIN statement's transaction went with the session; `begin` reports
    // it and, having opened nothing, leaves no transaction behind to end.
    let error = conn.begin().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SessionLost, "{error}");
    engine.reply([answer("s2", true), answer("s2", false), answer("s2", false)]);
    conn.execute("SELECT 1", &[]).unwrap();
    let last = sent(&engine).pop().unwrap();
    assert_eq!(last.session_id(), Some("s2"));
    assert_eq!(last.auto_commit(), Some(true));
}

#[test]
fn a_lost_context_is_reported_not_replaced() {
    for statement in [
        "USE SCHEMA OTHER",
        "SET v = 1",
        "UNSET v",
        "ALTER SESSION SET TIMEZONE = 'UTC'",
        "CREATE TEMPORARY TABLE tt (a INT)",
        "CREATE OR REPLACE DATABASE d2",
        "DROP SCHEMA IF EXISTS s2",
        "SELECT 1; USE SCHEMA OTHER",
    ] {
        let engine = ScriptedEngine::start();
        let mut conn = opened(&engine);
        engine.reply([answer("s1", false), gone("s1")]);
        conn.execute_with_multi_statement_count(statement, &[], Some(0)).unwrap();
        let error = conn.execute("SELECT * FROM T", &[]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::SessionLost, "{statement}: {error}");
        assert!(error.to_string().contains("context"), "{statement}: {error}");
        // Reported, not re-run somewhere else.
        assert_eq!(statements(&engine)[3..], [statement, "SELECT * FROM T"]);
    }
}

#[test]
fn a_lost_session_after_ordinary_ddl_is_replaced() {
    let engine = ScriptedEngine::start();
    let mut conn = opened(&engine);
    engine.reply([
        answer("s1", false),
        gone("s1"),
        answer("s2", true),
        answer("s2", false),
        answer("s2", false),
    ]);
    conn.execute("CREATE OR REPLACE TABLE T (a INT)", &[]).unwrap();
    conn.execute("INSERT INTO T VALUES (1)", &[]).unwrap();
    assert_eq!(engine.pending(), 0);
}

#[test]
fn a_replaced_session_gets_the_scope_back_before_the_next_statement() {
    let engine = ScriptedEngine::start();
    let mut conn =
        Connection::new(&format!("frostlake://127.0.0.1:{}/APP?schema=PUBLIC", engine.port)).unwrap();
    engine.reply([legacy("s1"), legacy("s1"), legacy("s1")]);
    conn.execute("SELECT 0", &[]).unwrap();
    // Not asked to require it, an engine that no longer holds the session runs
    // the statement in a fresh one under the same id, and says so.
    engine.reply([answer("s1", true), answer("s1", false), answer("s1", false), answer("s1", false)]);
    conn.execute("SELECT 1", &[]).unwrap();
    conn.execute("SELECT 2", &[]).unwrap();
    assert_eq!(
        statements(&engine),
        with_scope(&["SELECT 0", "SELECT 1", SCOPE[0], SCOPE[1], "SELECT 2"])
    );
    assert_eq!(sent(&engine).pop().unwrap().require_session(), Some(true));
}

#[test]
fn closing_releases_the_session_once() {
    let engine = ScriptedEngine::start();
    let mut conn = Connection::new(&format!("frostlake://127.0.0.1:{}", engine.port)).unwrap();
    engine.reply([answer("s1", true), released()]);
    conn.execute("SELECT 1", &[]).unwrap();
    conn.close();
    assert_eq!(deletes(&engine), ["/api/sessions/s1"]);
    conn.close();
    drop(conn);
    assert_eq!(engine.requests().len(), 2, "closing again sent something");
}

#[test]
fn dropping_an_open_connection_releases_its_session() {
    let engine = ScriptedEngine::start();
    {
        let mut conn = Connection::new(&format!("frostlake://127.0.0.1:{}", engine.port)).unwrap();
        engine.reply([answer("s1", true), released()]);
        conn.execute("SELECT 1", &[]).unwrap();
    }
    assert_eq!(deletes(&engine), ["/api/sessions/s1"]);
}

#[test]
fn closing_never_fails_whatever_the_release_meets() {
    let cases = [
        ("released", released()),
        (
            "unknown session",
            Reply::Answer(404, "{\"success\":false,\"sessionId\":null,\"errorMessage\":\"gone\"}".into()),
        ),
        ("method not allowed", Reply::Answer(405, "{\"error\":\"Method not allowed\"}".into())),
        ("not a Frostlake answer", Reply::Answer(502, "<html>Bad Gateway</html>".into())),
        ("closed socket", Reply::Drop),
        ("no answer", Reply::Hang),
    ];
    for (name, reply) in cases {
        let engine = ScriptedEngine::start();
        let mut conn = Connection::new(&format!("frostlake://127.0.0.1:{}", engine.port)).unwrap();
        conn.close_budget = Duration::from_millis(300);
        engine.reply([answer("s1", true), reply]);
        conn.execute("SELECT 1", &[]).unwrap();
        let started = Instant::now();
        conn.close();
        let took = started.elapsed();
        assert!(took < Duration::from_secs(3), "{name}: closing took {took:?}");
        assert_eq!(deletes(&engine).len(), 1, "{name}");
    }
}

#[test]
fn closing_without_a_session_sends_nothing() {
    let engine = ScriptedEngine::start();
    let mut conn =
        Connection::new(&format!("frostlake://127.0.0.1:{}/APP?schema=PUBLIC", engine.port)).unwrap();
    conn.close();
    assert!(engine.requests().is_empty());
}
