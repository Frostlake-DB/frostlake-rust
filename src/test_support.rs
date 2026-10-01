//! Canned-response loopback servers shared by the unit tests.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// How a [`ScriptedEngine`] answers one request.
pub(crate) enum Reply {
    /// Answer with this status and JSON body.
    Answer(u16, String),
    /// Close the socket without a word.
    Drop,
    /// Say nothing until the client gives up.
    Hang,
}

/// A loopback engine that answers each request — one connection each, the way
/// the driver sends them — from a script, and keeps every request it read. A
/// request the script has no answer for is kept too and answered with a 500, so
/// a test sees everything that was sent, scripted or not.
pub(crate) struct ScriptedEngine {
    pub(crate) port: u16,
    script: Arc<Mutex<VecDeque<Reply>>>,
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ScriptedEngine {
    pub(crate) fn start() -> ScriptedEngine {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let script: Arc<Mutex<VecDeque<Reply>>> = Arc::default();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (script, seen, stop) = (script.clone(), seen.clone(), stop.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(mut stream) = stream else { continue };
                    seen.lock().unwrap().push(read_request(&mut stream));
                    let reply = script.lock().unwrap().pop_front();
                    match reply {
                        Some(Reply::Answer(status, body)) => {
                            let _ = stream.write_all(raw_response(status, &body).as_bytes());
                        }
                        Some(Reply::Drop) => {}
                        Some(Reply::Hang) => {
                            let until = Instant::now() + Duration::from_secs(20);
                            while !stop.load(Ordering::SeqCst) && Instant::now() < until {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                        }
                        None => {
                            let body = "{\"success\":false,\"errorMessage\":\"unscripted\"}";
                            let _ = stream.write_all(raw_response(500, body).as_bytes());
                        }
                    }
                }
            })
        };
        ScriptedEngine { port, script, seen, stop, thread: Some(thread) }
    }

    /// Queues answers for the requests to come.
    pub(crate) fn reply(&self, replies: impl IntoIterator<Item = Reply>) {
        self.script.lock().unwrap().extend(replies);
    }

    /// Every request read so far, head and body.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    /// How many queued answers no request has asked for yet.
    pub(crate) fn pending(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

impl Drop for ScriptedEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop so it sees the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn raw_response(status: u16, body: &str) -> String {
    format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}
