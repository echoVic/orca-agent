//! The proxy the environment names is used, and `NO_PROXY` hosts are
//! reached directly. A process of its own, since it sets proxy variables.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

/// Answers one request with `body` and reports its request line.
fn serve_once(body: &'static str) -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let (seen, requests) = mpsc::channel();
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        stream.set_nonblocking(false).expect("blocking stream");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut request_line = String::new();
        reader.read_line(&mut request_line).expect("request line");
        let mut header = String::new();
        while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
            header.clear();
        }
        let _ = seen.send(request_line.trim_end().to_string());
        let mut stream = stream;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    });
    (port, requests)
}

#[test]
fn the_environment_proxy_is_used_and_no_proxy_hosts_are_reached_directly() {
    let (proxy_port, proxied) = serve_once("via proxy");
    let (direct_port, direct) = serve_once("direct");
    // SAFETY: this test binary runs this one test, before anything in it
    // reads the environment.
    unsafe {
        for name in [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "ALL_PROXY",
            "no_proxy",
        ] {
            std::env::remove_var(name);
        }
        std::env::set_var("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}"));
        std::env::set_var("NO_PROXY", "127.0.0.1");
    }
    let client = orca_mcp::http::blocking_client().expect("an HTTP client");

    let body = client
        .get("http://orca-proxy-probe.invalid/path")
        .send()
        .expect("proxied request")
        .text()
        .expect("body");
    assert_eq!(body, "via proxy");
    assert_eq!(
        proxied.recv().expect("the proxy saw a request"),
        "GET http://orca-proxy-probe.invalid/path HTTP/1.1"
    );

    let body = client
        .get(format!("http://127.0.0.1:{direct_port}/"))
        .send()
        .expect("direct request")
        .text()
        .expect("body");
    assert_eq!(body, "direct");
    assert_eq!(
        direct.recv().expect("the server saw a request"),
        "GET / HTTP/1.1"
    );
}
