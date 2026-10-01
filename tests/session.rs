//! Loses a connection's session behind its back — as the engine's idle reaper or
//! a restart would — against a real DatabaseHttpServer booted from
//! FROSTLAKE_CLASSPATH, and checks what the driver does next. Each test skips
//! itself when the variable is unset, and boots an engine of its own whose
//! guard stops it even on panic.

use frostlake::{connect, Connection, ErrorKind, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Boots an engine, returning its base DSN and port together with the guard
/// that stops it; `None` when FROSTLAKE_CLASSPATH names none.
fn engine() -> Option<(String, u16, ServerGuard)> {
    let Ok(classpath) = std::env::var("FROSTLAKE_CLASSPATH") else {
        eprintln!("skipping session test: FROSTLAKE_CLASSPATH not set");
        return None;
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
    let guard = ServerGuard(child);
    let base = format!("frostlake://127.0.0.1:{port}");
    for _ in 0..100 {
        if connect(&base).is_ok() {
            return Some((base, port, guard));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("server did not become healthy");
}

/// One raw request to the server, outside any connection: its status and body.
fn raw(port: u16, method: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    let status = text.split_whitespace().nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, body)| body.to_string()).unwrap_or_default();
    (status, body)
}

/// Ends a session behind its connection's back.
fn release(port: u16, session_id: &str) {
    let (status, body) = raw(port, "DELETE", &format!("/api/sessions/{session_id}"));
    assert_eq!(status, 200, "releasing session {session_id}: {body}");
}

fn active_sessions(port: u16) -> i64 {
    let (_, body) = raw(port, "GET", "/api/sessions");
    let digits: String = body
        .split_once("\"activeSessions\":")
        .map(|(_, rest)| rest.chars().take_while(char::is_ascii_digit).collect())
        .unwrap_or_default();
    digits.parse().unwrap_or_else(|_| panic!("no activeSessions in {body}"))
}

/// Makes the database, schema and table the tests name in their DSN, and opens
/// a connection on that scope.
fn scoped(base: &str) -> Connection {
    let mut setup = connect(base).unwrap();
    for sql in [
        "CREATE OR REPLACE DATABASE rs_lost_db",
        "CREATE OR REPLACE SCHEMA rs_lost_db.lost_schema",
        "CREATE OR REPLACE TABLE rs_lost_db.lost_schema.lost_t (a INTEGER)",
    ] {
        setup.execute(sql, &[]).unwrap();
    }
    setup.close();
    connect(&format!("{base}/RS_LOST_DB?schema=LOST_SCHEMA")).unwrap()
}

fn scope(conn: &mut Connection) -> Value {
    let result = conn.execute("SELECT CURRENT_DATABASE() || '.' || CURRENT_SCHEMA() AS s", &[]).unwrap();
    result.get(0, "S").cloned().unwrap()
}

fn dsn_scope() -> Value {
    Value::Str("RS_LOST_DB.LOST_SCHEMA".to_string())
}

#[test]
fn a_released_session_comes_back_on_the_dsn_scope() {
    let Some((base, port, _server)) = engine() else { return };
    let mut conn = scoped(&base);
    assert_eq!(scope(&mut conn), dsn_scope());
    let session_id = conn.session_id().unwrap().to_string();
    release(port, &session_id);
    assert_eq!(scope(&mut conn), dsn_scope(), "after its session was released the connection runs elsewhere");
    assert_ne!(conn.session_id(), Some(session_id.as_str()), "the connection still names the released session");
}

#[test]
fn a_released_session_under_a_transaction_is_reported() {
    let Some((base, port, _server)) = engine() else { return };
    let mut conn = scoped(&base);
    assert_eq!(scope(&mut conn), dsn_scope());
    conn.begin().unwrap();
    conn.execute("INSERT INTO lost_t VALUES (1)", &[]).unwrap();
    release(port, conn.session_id().unwrap());
    let error = conn.execute("INSERT INTO lost_t VALUES (2)", &[]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SessionLost, "{error}");
    let commit = conn.commit().unwrap_err();
    assert_eq!(commit.kind(), ErrorKind::SessionLost, "{commit}");
    // The connection stays usable, on the DSN's scope, and nothing of the
    // transaction survived.
    assert_eq!(scope(&mut conn), dsn_scope());
    let count = conn.execute("SELECT COUNT(*) AS n FROM lost_t", &[]).unwrap();
    assert_eq!(count.get(0, "N"), Some(&Value::Int(0)));
}

#[test]
fn closing_releases_the_engine_session() {
    let Some((base, port, _server)) = engine() else { return };
    let mut conn = connect(&base).unwrap();
    conn.execute("SELECT 1", &[]).unwrap();
    let before = active_sessions(port);
    conn.close();
    assert_eq!(active_sessions(port), before - 1, "closing left the session active");
}
