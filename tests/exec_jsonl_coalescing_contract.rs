use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct ScriptedEndpoint {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<io::Result<Vec<Value>>>>,
}

impl ScriptedEndpoint {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            while !worker_stop.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                // Accepted BSD sockets can inherit O_NONBLOCK from the listener.
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                requests.push(read_request(&stream)?);
                let mut body = String::new();
                for delta in (0..200)
                    .map(|_| json!({"reasoning_content": " 思考\n\t🦀 "}))
                    .chain(
                        ["Hello", " ", "世界", "!\n"]
                            .into_iter()
                            .map(|text| json!({"content": text})),
                    )
                    .chain(std::iter::once(json!({})))
                {
                    let finish = delta.as_object().unwrap().is_empty().then_some("stop");
                    let chunk = json!({
                        "id": "coalescing-contract", "object": "chat.completion.chunk",
                        "model": "deepseek-flash",
                        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
                    });
                    body.push_str(&format!("data: {chunk}\n\n"));
                }
                body.push_str("data: [DONE]\n\n");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
                stream.flush()?;
            }
            Ok(requests)
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(&mut self) -> Vec<Value> {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap()
    }
}

impl Drop for ScriptedEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &TcpStream) -> io::Result<Value> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    assert!(line.starts_with("POST ") && line.contains("/chat/completions"));
    let mut length = None;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::other("incomplete HTTP headers"));
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
        }
    }
    let length = length.ok_or_else(|| io::Error::other("missing content-length"))?;
    assert!(length < 4 * 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

fn run_exec(format: &str) -> String {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut endpoint = ScriptedEndpoint::start();
    let mut child = Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(cwd.path())
        .env("ORCA_HOME", home.path())
        .env("ORCA_API_KEY", "local-contract-fixture")
        .env("DEEPSEEK_API_KEY", "local-contract-fixture")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args([
            "exec",
            "--provider",
            "deepseek",
            "--mode",
            "full-auto",
            "--model",
            "deepseek-flash",
            "--base-url",
            &endpoint.url,
            "--output-format",
            format,
            "--max-turns",
            "4",
            "--max-tool-calls",
            "4",
            "--max-wall-time-secs",
            "20",
            "Create snake.py and run the command.",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let read = |mut pipe: Box<dyn Read + Send>| {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        bytes
    };
    let out = thread::spawn(move || read(Box::new(stdout)));
    let err = thread::spawn(move || read(Box::new(stderr)));
    let deadline = Instant::now() + Duration::from_secs(45);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let status = child.wait().unwrap();
    let stdout = String::from_utf8(out.join().unwrap()).unwrap();
    let stderr = String::from_utf8(err.join().unwrap()).unwrap();
    let requests = endpoint.finish();
    assert!(!timed_out, "CLI timed out: {stderr}\n{stdout}");
    assert!(status.success(), "{stderr}\n{stdout}");
    assert_eq!(requests.len(), 1);
    stdout
}

#[test]
fn exec_jsonl_coalesces_streamed_deltas_without_losing_response_text() {
    let stdout = run_exec("jsonl");
    let events: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (kind, field, expected) in [
        (
            "assistant.reasoning.delta",
            "assistant_reasoning",
            " 思考\n\t🦀 ".repeat(200),
        ),
        (
            "assistant.message.delta",
            "assistant_content",
            "Hello 世界!\n".to_string(),
        ),
    ] {
        let deltas: Vec<_> = events.iter().filter(|e| e["type"] == kind).collect();
        assert!(
            !deltas.is_empty() && deltas.len() <= 3,
            "{kind}: {} events",
            deltas.len()
        );
        let text: String = deltas
            .iter()
            .map(|e| e["payload"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(text, expected);
        let completed = events
            .iter()
            .find(|e| e["type"] == "model.response.completed")
            .unwrap();
        assert_eq!(completed["payload"][field], text);
    }
    assert_eq!(events.last().unwrap()["type"], "session.completed");
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0]["seq"].as_u64() < pair[1]["seq"].as_u64())
    );
}

#[test]
fn exec_text_preserves_each_streamed_delta() {
    let stdout = run_exec("text");
    assert_eq!(stdout.matches("thinking:  思考\n\t🦀 ").count(), 200);
    for text in ["Hello", " ", "世界", "!\n"] {
        assert!(stdout.contains(&format!("assistant: {text}\n")), "{stdout}");
    }
}
