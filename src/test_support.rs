//! Canned-response loopback servers shared by the unit tests.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

/// Reads one whole HTTP request — head plus Content-Length body — however the
/// client's bytes were segmented on the way.
pub(crate) fn read_request(stream: &mut TcpStream) -> String {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(text) = complete_request(&data) {
            return text;
        }
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            return String::from_utf8_lossy(&data).into_owned();
        }
        data.extend_from_slice(&buf[..n]);
    }
}

fn complete_request(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data).into_owned();
    let head_end = text.find("\r\n\r\n")?;
    let mut content_length = 0usize;
    for line in text[..head_end].lines() {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    (data.len() >= head_end + 4 + content_length).then_some(text)
}

/// Answers each connection accepted on `listener` with the next canned raw
/// response — one connection per request, the way the driver's
/// `Connection: close` protocol works — and yields the requests it read.
pub(crate) fn serve_on(listener: TcpListener, responses: Vec<String>) -> JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            seen.push(read_request(&mut stream));
            stream.write_all(response.as_bytes()).unwrap();
        }
        seen
    })
}

/// [`serve_on`] over a fresh 127.0.0.1 listener; returns its port too.
pub(crate) fn serve(responses: Vec<String>) -> (u16, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (port, serve_on(listener, responses))
}
