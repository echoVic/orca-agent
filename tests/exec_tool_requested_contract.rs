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
                let delta = if requests.len() == 1 {
                    json!({
                        "reasoning_content": "Create the file and run the command.",
                        "tool_calls": [
                            {"index": 0, "id": "call-write", "type": "function",
                             "function": {"name": "write_file", "arguments": r#"{"path":"snake.py","content":"print(1)\n"}"#}},
                            {"index": 1, "id": "call-bash", "type": "function",
                             "function": {"name": "bash", "arguments": r#"{"command":"printf contract-ok"}"#}}
                        ]
                    })
                } else {
                    json!({"content": "Done."})
                };
                let finish = if delta.get("tool_calls").is_some() {
                    "tool_calls"
                } else {
                    "stop"
                };
                let chunk = json!({
                    "id": "requested-contract", "object": "chat.completion.chunk",
                    "model": "deepseek-flash",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
                });
                let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
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

#[test]
fn server_deepseek_requested_events_report_canonical_action_and_target() {
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{HistoryMode, ProviderKind, RunConfig};
    use orca_core::model::ModelSelection;
    use orca_runtime::server::JsonlSurfaceAdapter;

    let home = tempfile::tempdir().unwrap();
    let previous_home = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };
    let cwd = tempfile::tempdir().unwrap();
    let mut endpoint = ScriptedEndpoint::start();
    let config = RunConfig {
        cwd: Some(cwd.path().to_path_buf()),
        provider: ProviderKind::DeepSeek,
        model: ModelSelection::parse(Some("deepseek-flash".to_string())).unwrap(),
        api_key: Some("local-contract-fixture".to_string()),
        base_url: Some(endpoint.url.clone()),
        approval_mode: ApprovalMode::FullAuto,
        history_mode: HistoryMode::Disabled,
        ..RunConfig::default()
    };
    let mut runtime = JsonlSurfaceAdapter::start().unwrap();
    let thread_id = runtime.start_thread(&config).unwrap();
    let mut output = Vec::new();
    runtime
        .run_turn(
            &config,
            &thread_id,
            "Create snake.py and run the command.",
            &mut output,
        )
        .unwrap();
    runtime.shutdown().unwrap();
    unsafe {
        if let Some(previous) = previous_home {
            std::env::set_var("ORCA_HOME", previous);
        } else {
            std::env::remove_var("ORCA_HOME");
        }
    }
    assert!(endpoint.finish().len() >= 2);

    let events = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let requested = events
        .iter()
        .find(|event| {
            event["type"] == "tool.call.requested" && event["payload"]["id"] == "call-write"
        })
        .expect("server projects the committed write request");
    assert_eq!(requested["payload"]["name"], "write_file");
    assert_eq!(requested["payload"]["target"], "snake.py");
    assert_eq!(
        requested["payload"]["raw_arguments"],
        r#"{"path":"snake.py","content":"print(1)\n"}"#
    );
    assert_eq!(requested["payload"]["action"], "write");
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("snake.py")).unwrap(),
        "print(1)\n"
    );
}

#[test]
fn exec_deepseek_requested_events_report_canonical_action_and_target() {
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
            "jsonl",
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
    assert!(
        requests.len() >= 2,
        "provider must receive the tool results"
    );
    let events = stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let requested = events
        .iter()
        .filter(|event| event["type"] == "tool.call.requested")
        .collect::<Vec<_>>();
    assert_eq!(requested.len(), 2, "{stdout}");
    for (id, name, action, target, arguments) in [
        (
            "call-write",
            "write_file",
            "write",
            "snake.py",
            r#"{"path":"snake.py","content":"print(1)\n"}"#,
        ),
        (
            "call-bash",
            "bash",
            "shell",
            "printf contract-ok",
            r#"{"command":"printf contract-ok"}"#,
        ),
    ] {
        let payload = &requested
            .iter()
            .find(|event| event["payload"]["id"] == id)
            .unwrap()["payload"];
        assert_eq!(payload["name"], name);
        assert_eq!(payload["action"], action, "{name}");
        assert_eq!(payload["target"], target, "{name}");
        assert_eq!(payload["raw_arguments"], arguments);
    }
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("snake.py")).unwrap(),
        "print(1)\n"
    );
    assert!(events.iter().any(|event| {
        event["type"] == "tool.call.completed"
            && event["payload"]["id"] == "call-bash"
            && event["payload"]["output"]
                .as_str()
                .is_some_and(|s| s.contains("contract-ok"))
    }));
}
