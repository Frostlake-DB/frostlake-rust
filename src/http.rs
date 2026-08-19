//! A minimal HTTP/1.1 client over `TcpStream` — the Frostlake protocol is
//! plaintext HTTP against a local server, so the crate stays dependency-free.
//! One connection per request (`Connection: close`); bodies by Content-Length,
//! chunked transfer coding, or read-to-EOF.

use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// How long each resolved address may take to accept: a plain connect waits
/// out the OS's SYN retries — about two minutes on Linux — for a host that
/// never answers.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a statement's answer is awaited.
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

pub fn request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    json_body: Option<&str>,
) -> io::Result<Response> {
    request_within(host, port, method, path, json_body, READ_TIMEOUT)
}

fn request_within(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    json_body: Option<&str>,
    read_timeout: Duration,
) -> io::Result<Response> {
    let stream = connect(host, port)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;

    // An IPv6 literal is bracketed in the Host header, URL-style.
    let host_header = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    let mut message =
        format!("{method} {path} HTTP/1.1\r\nHost: {host_header}:{port}\r\nConnection: close\r\n");
    if let Some(payload) = json_body {
        message.push_str("Content-Type: application/json\r\n");
        message.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    message.push_str("\r\n");
    message.push_str(json_body.unwrap_or(""));
    // Head and body leave in one write: a body sent as a second small segment
    // waits behind Nagle until the head is acknowledged — a round trip per
    // statement against a remote server.
    (&stream).write_all(message.as_bytes())?;

    read_response(BufReader::new(stream)).map_err(|e| match e.kind() {
        // An expired read timeout surfaces as WouldBlock on Unix, TimedOut on Windows.
        ErrorKind::WouldBlock | ErrorKind::TimedOut => io::Error::new(
            ErrorKind::TimedOut,
            format!("no response from the server within {read_timeout:?}"),
        ),
        _ => e,
    })
}

/// Tries each address `host` resolves to in turn, each for at most
/// `CONNECT_TIMEOUT`.
fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut last_error = None;
    for address in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(ErrorKind::NotFound, format!("{host} resolves to no address"))
    }))
}

fn read_response(mut reader: BufReader<TcpStream>) -> io::Result<Response> {
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other(format!("bad status line: {status_line:?}")))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse().ok();
            } else if name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }

    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line)?;
            // The size may carry a ";ext=..." chunk extension — parse up to it.
            let digits = size_line.trim().split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(digits, 16)
                .map_err(|_| io::Error::other(format!("bad chunk size: {size_line:?}")))?;
            if size == 0 {
                let mut trailer = String::new();
                reader.read_line(&mut trailer)?;
                break;
            }
            let mut chunk = vec![0u8; size];
            reader.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf)?;
        }
    } else if let Some(length) = content_length {
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    } else {
        reader.read_to_end(&mut body)?;
    }

    Ok(Response {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::{request, request_within};
    use crate::test_support::{read_request, serve, serve_on};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn reads_content_length_bodies() {
        let (port, server) = serve(vec!["HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".into()]);
        let response = request("127.0.0.1", port, "GET", "/api/health", None).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "hello");
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("GET /api/health HTTP/1.1\r\n"), "{}", seen[0]);
        assert!(seen[0].contains("\r\nHost: 127.0.0.1:"), "{}", seen[0]);
    }

    #[test]
    fn reads_chunked_bodies_with_extensions() {
        let (port, server) = serve(vec![
            "HTTP/1.1 500 Server Error\r\nTransfer-Encoding: chunked\r\n\r\n4;ext=1\r\nab\r\n\r\n3\r\ncde\r\n0\r\n\r\n".into(),
        ]);
        let response = request("127.0.0.1", port, "POST", "/api/execute", Some("{}")).unwrap();
        assert_eq!(response.status, 500);
        assert_eq!(response.body, "ab\r\ncde");
        let seen = server.join().unwrap();
        assert!(seen[0].contains("\r\nContent-Length: 2\r\n"), "{}", seen[0]);
        assert!(seen[0].ends_with("\r\n\r\n{}"), "{}", seen[0]);
    }

    #[test]
    fn sends_the_request_in_one_segment() {
        // The server's very first read holds the whole request — head and body
        // left in one write, so no second segment waits behind Nagle.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        request("127.0.0.1", port, "POST", "/api/execute", Some("{\"sql\":\"SELECT 1\"}")).unwrap();
        let first_read = server.join().unwrap();
        assert!(first_read.ends_with("\r\n\r\n{\"sql\":\"SELECT 1\"}"), "{first_read}");
    }

    #[test]
    fn reads_to_eof_without_length() {
        let (port, _server) =
            serve(vec!["HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nrest of stream".into()]);
        let response = request("127.0.0.1", port, "GET", "/", None).unwrap();
        assert_eq!(response.body, "rest of stream");
    }

    #[test]
    fn rejects_a_bad_status_line() {
        let (port, _server) = serve(vec!["garbage\r\n\r\n".into()]);
        assert!(request("127.0.0.1", port, "GET", "/", None).is_err());
    }

    #[test]
    fn a_silent_server_times_out_with_a_clear_message() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            std::thread::sleep(Duration::from_secs(2));
        });
        let error = request_within("127.0.0.1", port, "GET", "/", None, Duration::from_millis(200))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "no response from the server within 200ms");
    }

    #[test]
    fn brackets_ipv6_hosts_in_the_host_header() {
        // Self-skips where the loopback has no IPv6.
        let Ok(listener) = TcpListener::bind("[::1]:0") else {
            eprintln!("skipping IPv6 test: cannot bind [::1]");
            return;
        };
        let port = listener.local_addr().unwrap().port();
        let server = serve_on(listener, vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".into()]);
        let response = request("::1", port, "GET", "/api/health", None).unwrap();
        assert_eq!(response.body, "ok");
        let seen = server.join().unwrap();
        assert!(seen[0].contains(&format!("\r\nHost: [::1]:{port}\r\n")), "{}", seen[0]);
    }
}
