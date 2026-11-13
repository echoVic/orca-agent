//! A local HTTP server for tool tests. It answers each expected request with
//! a canned response and hands back the raw requests it saw.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Fixture {
    /// `http://127.0.0.1:<port>`, without a trailing slash.
    pub(crate) origin: String,
    handle: JoinHandle<Vec<String>>,
}

impl Fixture {
    /// The raw requests the server received, in order. A request that never
    /// arrived within the accept timeout ends the list early.
    pub(crate) fn requests(self) -> Vec<String> {
        self.handle.join().expect("fixture server thread")
    }
}

/// Serves one connection per `(status, body)` pair, in order, each with a
/// JSON content type and `connection: close`.
pub(crate) fn serve(responses: Vec<(u16, String)>) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    listener
        .set_nonblocking(true)
        .expect("non-blocking fixture listener");
    let origin = format!(
        "http://{}",
        listener.local_addr().expect("fixture server address")
    );
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let Some(mut stream) = accept_within(&listener, ACCEPT_TIMEOUT) else {
                break;
            };
            requests.push(read_request(&mut stream));
            let reply = format!(
                "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
        requests
    });
    Fixture { origin, handle }
}

/// The body of a raw HTTP request.
pub(crate) fn body(request: &str) -> &str {
    request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

fn accept_within(listener: &TcpListener, timeout: Duration) -> Option<TcpStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // On macOS an accepted socket inherits the listener's O_NONBLOCK.
                stream
                    .set_nonblocking(false)
                    .expect("blocking fixture stream");
                stream
                    .set_read_timeout(Some(READ_TIMEOUT))
                    .expect("fixture read timeout");
                return Some(stream);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

/// Reads the request line, the headers and a `content-length` body.
fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
        let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if buffer.len() >= header_end + 4 + length {
            break;
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}
