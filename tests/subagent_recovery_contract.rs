//! Exercise the production CLI, hosted parent, provider transport, and child
//! checkpoint path. Only model responses are scripted; no runtime is mocked.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TOKEN: &str = "FROZEN_DEFINITION_ONLY_RECOVERY_TOKEN";

struct Response {
    delta: Value,
    child: bool,
    resumed: bool,
    wait_for_child: bool,
}

struct ScriptedEndpoint {
    url: String,
    pending: Arc<Mutex<VecDeque<Response>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<io::Result<()>>>,
}

impl ScriptedEndpoint {
    fn start(home: &Path) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let pending = Arc::new(Mutex::new(VecDeque::<Response>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_pending = pending.clone();
        let worker_requests = requests.clone();
        let worker_stop = stop.clone();
        let task_sessions = home.join("task-sessions");
        let worker = thread::spawn(move || {
            let mut delayed = Vec::new();
            while !worker_stop.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                // BSD sockets may inherit O_NONBLOCK from the listener.
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                let request = read_request(&stream)?;
                worker_requests.lock().unwrap().push(request.clone());
                let is_child = request.get("tools").is_none_or(|tools| {
                    tools.is_null() || tools.as_array().is_some_and(Vec::is_empty)
                });
                let response = {
                    let mut pending = worker_pending.lock().unwrap();
                    let index = pending
                        .iter()
                        .position(|response| response.child == is_child)
                        .ok_or_else(|| io::Error::other("unexpected model request"))?;
                    pending.remove(index).unwrap()
                };
                let messages = request["messages"]
                    .as_array()
                    .ok_or_else(|| io::Error::other("missing model messages"))?;
                if response.child {
                    assert!(
                        messages.iter().any(|message| {
                            message["role"] == "system"
                                && message["content"]
                                    .as_str()
                                    .is_some_and(|s| s.contains(TOKEN))
                        }),
                        "child must receive the definition-only instructions"
                    );
                    assert!(
                        request.get("tools").is_none_or(|tools| {
                            tools.is_null() || tools.as_array().is_some_and(Vec::is_empty)
                        }),
                        "tools: [] must remain empty on both child requests"
                    );
                    assert_eq!(
                        messages.iter().any(|message| {
                            message["role"] == "assistant" && message["content"] == TOKEN
                        }),
                        response.resumed,
                        "resumed child must restore its own checkpoint"
                    );
                } else {
                    assert!(
                        !messages.iter().any(|message| {
                            message["role"] == "system"
                                && message["content"]
                                    .as_str()
                                    .is_some_and(|s| s.contains(TOKEN))
                        }),
                        "the custom body must not leak into the parent's system prompt"
                    );
                }
                let finish = if response.delta.get("tool_calls").is_some() {
                    "tool_calls"
                } else {
                    "stop"
                };
                let chunk = json!({
                    "id": "recovery-fixture",
                    "object": "chat.completion.chunk",
                    "model": "deepseek-flash",
                    "choices": [{"index": 0, "delta": response.delta, "finish_reason": finish}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
                });
                let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                if response.wait_for_child {
                    let launch = messages
                        .iter()
                        .rev()
                        .filter_map(|message| {
                            (message["role"] == "tool")
                                .then(|| {
                                    serde_json::from_str::<Value>(message["content"].as_str()?).ok()
                                })
                                .flatten()
                        })
                        .find(|value| value["status"] == "async_launched")
                        .ok_or_else(|| io::Error::other("missing async launch result"))?;
                    let sessions = task_sessions.clone();
                    delayed.push(thread::spawn(move || {
                        let selector = launch["continuation_id"].as_str().unwrap();
                        let attempt = &launch["attempt_id"];
                        let deadline = Instant::now() + Duration::from_secs(20);
                        loop {
                            let record =
                                fs::read_dir(&sessions)?
                                    .filter_map(Result::ok)
                                    .find_map(|entry| {
                                        let bytes = fs::read(
                                            entry
                                                .path()
                                                .join("continuations")
                                                .join(format!("{selector}.json")),
                                        )
                                        .ok()?;
                                        serde_json::from_slice::<Value>(&bytes).ok()
                                    });
                            if let Some(record) = record
                                && record["current_attempt"]["attempt_id"] == *attempt
                                && record["terminal"].is_object()
                            {
                                assert_eq!(record["terminal"]["status"], "completed", "{record}");
                                break;
                            }
                            if Instant::now() >= deadline {
                                return Err(io::Error::other("async child did not settle"));
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        write_response(&mut stream, &body)
                    }));
                } else {
                    write_response(&mut stream, &body)?;
                }
            }
            for worker in delayed {
                worker
                    .join()
                    .map_err(|_| io::Error::other("delayed response panicked"))??;
            }
            Ok(())
        });
        Self {
            url,
            pending,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn enqueue(&self, selector: Option<&str>, rejected: bool) {
        self.enqueue_mode(selector, rejected, "sync");
    }

    fn enqueue_mode(&self, selector: Option<&str>, rejected: bool, mode: &str) {
        assert!(self.pending.lock().unwrap().is_empty());
        let mut arguments = json!({
            "description": "Verify frozen configuration",
            "prompt": "Complete your assigned verification",
            "mode": mode
        });
        if let Some(selector) = selector {
            arguments["resume_from"] = json!(selector);
        } else {
            arguments["subagent_type"] = json!("contract-proof");
        }
        if rejected {
            // Reach continuation ownership validation even when the source
            // is foreign and cannot be used to infer a custom-agent route.
            arguments["subagent_type"] = json!("contract-proof");
        }
        let mut pending = self.pending.lock().unwrap();
        pending.push_back(Response {
            delta: json!({
                "reasoning_content": "Delegate the verification.",
                "tool_calls": [{
                    "index": 0, "id": format!("call-{}", uuid::Uuid::new_v4()),
                    "type": "function",
                    "function": {"name": "subagent", "arguments": arguments.to_string()}
                }]
            }),
            child: false,
            resumed: false,
            wait_for_child: false,
        });
        if !rejected {
            pending.push_back(Response {
                delta: json!({"content": TOKEN}),
                child: true,
                resumed: selector.is_some(),
                wait_for_child: false,
            });
            pending.push_back(Response {
                delta: json!({"content": TOKEN}),
                child: false,
                resumed: false,
                wait_for_child: mode == "async",
            });
        }
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap();
        assert!(self.pending.lock().unwrap().is_empty());
    }
}

fn write_response(stream: &mut TcpStream, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

impl Drop for ScriptedEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && let Ok(Err(error)) = worker.join()
        {
            eprintln!("scripted endpoint failed: {error}");
        }
        if thread::panicking() {
            eprintln!(
                "scripted endpoint requests: {}, pending responses: {}",
                self.requests.lock().unwrap().len(),
                self.pending.lock().unwrap().len()
            );
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
    assert!(length < 4 * 1024 * 1024, "unexpected fixture request size");
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

fn run_cli(home: &Path, cwd: &Path, endpoint: &str, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
    command
        .current_dir(cwd)
        .env("ORCA_HOME", home)
        .env("ORCA_API_KEY", "local-recovery-fixture")
        .env("DEEPSEEK_API_KEY", "local-recovery-fixture")
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
            endpoint,
            "--output-format",
            "jsonl",
            "--save-history",
            "--max-turns",
            "5",
            "--max-tool-calls",
            "5",
            "--max-wall-time-secs",
            "30",
        ])
        .args(extra)
        .arg("Call the specified subagent once and relay its reply.")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let read = |mut pipe: Box<dyn Read + Send>| {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        bytes
    };
    let out = thread::spawn(move || read(Box::new(stdout)));
    let err = thread::spawn(move || read(Box::new(stderr)));
    let deadline = Instant::now() + Duration::from_secs(60);
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
    let output = Output {
        status: child.wait().unwrap(),
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    };
    assert!(!timed_out, "CLI recovery timed out: {output:?}");
    output
}

fn completed_child(output: &Output) -> (String, String) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let results: Vec<_> = events
        .iter()
        .filter(|event| {
            event["type"] == "tool.call.completed" && event["payload"]["name"] == "subagent"
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["payload"]["status"], "completed");
    let text = results[0]["payload"]["output"].as_str().unwrap();
    let selector = if let Ok(launch) = serde_json::from_str::<Value>(text) {
        assert_eq!(launch["status"], "async_launched");
        launch["continuation_id"].as_str().unwrap().to_string()
    } else {
        assert!(text.contains(TOKEN));
        text.lines()
            .find_map(|line| line.strip_prefix("resume_from="))
            .unwrap()
            .to_string()
    };
    let session = events
        .iter()
        .find(|event| event["type"] == "session.completed")
        .unwrap()["payload"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    (session, selector)
}

fn assert_parent_admission(home: &Path, record: &Value) {
    let suffix = format!("{}.jsonl", record["parent_session_id"].as_str().unwrap());
    let mut directories = vec![home.join("sessions")];
    let path = 'search: loop {
        let directory = directories.pop().expect("recorded parent session");
        for entry in fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                directories.push(entry.path());
            } else if entry.file_name().to_string_lossy().ends_with(&suffix) {
                break 'search entry.path();
            }
        }
    };
    let records: Vec<Value> = BufReader::new(fs::File::open(path).unwrap())
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    let committed: std::collections::HashSet<_> = records
        .iter()
        .filter(|entry| entry["type"] == "runtime.surface_commit.committed")
        .map(|entry| entry["commit_id"].as_str().unwrap())
        .collect();
    let events: Vec<_> = records
        .iter()
        .filter(|entry| {
            entry["type"] == "runtime.surface_commit.prepared"
                && committed.contains(entry["commit_id"].as_str().unwrap())
        })
        .flat_map(|entry| entry["batch"]["events"].as_array().unwrap())
        .collect();
    let child = events
        .iter()
        .map(|entry| &entry["event"]["Subagent"]["Started"]["subagent"])
        .find(|child| child["task_id"] == record["latest_task_id"])
        .expect("committed latest child admission");
    let task = events
        .iter()
        .map(|entry| &entry["event"]["Task"]["Upserted"]["task"])
        .find(|task| task["task_id"] == record["latest_task_id"])
        .unwrap();
    let admission = events
        .iter()
        .map(|entry| &entry["event"]["Operation"]["AgentLoopTurnStarted"]["turn"])
        .find(|turn| {
            turn["fence"]["operation_id"] == task["parent_operation"]
                && turn["admitted_main_task_id"] == record["parent_task_id"]
        })
        .expect("committed parent generation admission");
    assert_eq!(admission["admitted_main_task_id"], record["parent_task_id"]);
    assert_ne!(admission["task_id"], record["parent_task_id"]);
    assert_eq!(
        child["continuation"]["attempt_id"],
        record["current_attempt"]["attempt_id"]
    );
    assert_eq!(
        child["continuation"]["continuation_id"],
        record["continuation_id"]
    );
    if record["source_task_id"] == record["latest_task_id"] {
        let expected_revision = u64::from(child["owner"]["Generation"].is_object());
        assert_eq!(child["continuation"]["revision"], expected_revision);
    }
    let executed_turn = records
        .iter()
        .map(|entry| &entry["event"])
        .find(|event| {
            event["type"] == "turn.started" && event["payload"]["turn_id"] == admission["turn_id"]
        })
        .expect("executed parent turn");
    assert_eq!(
        executed_turn["payload"]["task"]["task_id"], admission["admitted_main_task_id"],
        "the runner must execute the actor-admitted registry root"
    );
}

#[test]
fn custom_agent_exec_continue_recovers_original_owner_and_rejects_other_sessions() {
    recovery_contract("sync");
}

#[test]
fn detached_custom_agent_recovers_across_modes_and_rejects_other_sessions() {
    recovery_contract("async");
}

fn recovery_contract(initial_mode: &str) {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("agents")).unwrap();
    fs::write(
        home.path().join("config.toml"),
        "update_check = false\nauto_memory = false\n",
    )
    .unwrap();
    let definition = home.path().join("agents/contract-proof.md");
    fs::write(&definition, format!(
        "---\nname: contract-proof\ndescription: Verify immutable configuration\ntools: []\nmodel: deepseek-flash\n---\nAlways answer with exactly {TOKEN}.\n"
    )).unwrap();
    let mut endpoint = ScriptedEndpoint::start(home.path());
    endpoint.enqueue_mode(None, false, initial_mode);
    let first = run_cli(home.path(), cwd.path(), &endpoint.url, &[]);
    let (session, selector) = completed_child(&first);
    let record_path = home
        .path()
        .join("task-sessions")
        .join(&session)
        .join("continuations")
        .join(format!("{selector}.json"));
    let original: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    assert!(
        original["parent_task_id"].is_string(),
        "real parent task must be bound"
    );
    assert_parent_admission(home.path(), &original);
    fs::remove_file(definition).unwrap();

    // This is the failing production boundary: new process, same saved parent,
    // a newly allocated main-session task, and no definition left to discover.
    let mut previous = original.clone();
    for (mode, extra) in [
        ("sync", vec!["--continue"]),
        ("async", vec!["--resume", session.as_str()]),
        ("async", vec!["--continue"]),
        ("sync", vec!["--resume", session.as_str()]),
    ] {
        endpoint.enqueue_mode(Some(&selector), false, mode);
        let resumed = run_cli(home.path(), cwd.path(), &endpoint.url, &extra);
        assert_eq!(
            completed_child(&resumed),
            (session.clone(), selector.clone())
        );
        let record: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        assert_eq!(record["terminal"]["status"], "completed");
        assert_eq!(record["terminal"]["result"], TOKEN);
        assert_parent_admission(home.path(), &record);
        assert_eq!(record["source_task_id"], original["source_task_id"]);
        assert_eq!(record["parent_session_id"], original["parent_session_id"]);
        assert_eq!(record["compatibility_hash"], original["compatibility_hash"]);
        assert_eq!(record["frozen_agent"], original["frozen_agent"]);
        assert_ne!(record["parent_task_id"], previous["parent_task_id"]);
        assert_ne!(record["latest_task_id"], previous["latest_task_id"]);
        assert_ne!(
            record["current_attempt"]["attempt_id"],
            previous["current_attempt"]["attempt_id"]
        );
        assert_eq!(
            record["current_attempt"]["resumed_from_attempt_id"],
            previous["current_attempt"]["attempt_id"]
        );
        assert!(record["revision"].as_u64().unwrap() > previous["revision"].as_u64().unwrap());
        assert!(
            record["checkpoint"]["sequence"].as_u64().unwrap()
                > previous["checkpoint"]["sequence"].as_u64().unwrap()
        );
        let registry = orca_runtime::tasks::TaskRegistry::new_persistent(
            session.clone(),
            home.path().join("task-sessions"),
        )
        .unwrap();
        let source = registry
            .get(original["source_task_id"].as_str().unwrap())
            .unwrap();
        let latest = registry
            .get(record["latest_task_id"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            source.parent_task_id.as_deref(),
            original["parent_task_id"].as_str()
        );
        assert_eq!(
            latest.parent_task_id.as_deref(),
            record["parent_task_id"].as_str()
        );
        if mode == "async" {
            let bindings_path = home
                .path()
                .join("task-sessions")
                .join(&session)
                .join("detached-subagent-bindings.json");
            let bindings_bytes = fs::read(&bindings_path).unwrap();
            let settled = fs::read(&record_path).unwrap();
            for (field, value) in [
                (
                    "attempt_id",
                    original["current_attempt"]["attempt_id"].clone(),
                ),
                ("authority_digest", original["compatibility_hash"].clone()),
            ] {
                let mut bindings: Value = serde_json::from_slice(&bindings_bytes).unwrap();
                bindings[record["latest_task_id"].as_str().unwrap()][field] = value;
                fs::write(
                    &bindings_path,
                    serde_json::to_vec_pretty(&bindings).unwrap(),
                )
                .unwrap();
                let before = endpoint.requests.lock().unwrap().len();
                endpoint.enqueue(Some(&selector), true);
                let rejected = run_cli(
                    home.path(),
                    cwd.path(),
                    &endpoint.url,
                    &["--resume", &session],
                );
                assert!(
                    !rejected.status.success(),
                    "forged detached {field} was accepted"
                );
                assert!(
                    String::from_utf8_lossy(&rejected.stdout)
                        .contains("continuation task binding mismatch"),
                    "{rejected:?}"
                );
                assert_eq!(endpoint.requests.lock().unwrap().len(), before + 1);
                assert_eq!(fs::read(&record_path).unwrap(), settled);
                fs::write(&bindings_path, &bindings_bytes).unwrap();
            }
        }
        previous = record;
    }

    let settled = fs::read(&record_path).unwrap();
    // A same-session caller still cannot substitute the preceding task root
    // or manufacture original ownership from an arbitrary task selector.
    for (mode, field, value) in ["sync", "async"].into_iter().flat_map(|mode| {
        [
            ("parent_task_id", json!("unowned-parent-task")),
            ("parent_task_id", original["parent_task_id"].clone()),
            ("source_task_id", json!("unrecorded-source-task")),
            ("source_task_id", previous["latest_task_id"].clone()),
            ("latest_task_id", original["source_task_id"].clone()),
        ]
        .into_iter()
        .map(move |(field, value)| (mode, field, value))
    }) {
        let mut corrupted: Value = serde_json::from_slice(&settled).unwrap();
        corrupted[field] = value;
        let bytes = serde_json::to_vec_pretty(&corrupted).unwrap();
        fs::write(&record_path, &bytes).unwrap();
        let before = endpoint.requests.lock().unwrap().len();
        endpoint.enqueue_mode(Some(&selector), true, mode);
        let rejected = run_cli(
            home.path(),
            cwd.path(),
            &endpoint.url,
            &["--resume", &session],
        );
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stdout)
                .contains("continuation task binding mismatch"),
            "unproven {field} must fail closed: {rejected:?}"
        );
        assert_eq!(endpoint.requests.lock().unwrap().len(), before + 1);
        assert_eq!(fs::read(&record_path).unwrap(), bytes);
        fs::write(&record_path, &settled).unwrap();
    }
    for (mode, extra) in ["sync", "async"]
        .into_iter()
        .flat_map(|mode| [vec![], vec!["--fork", session.as_str()]].map(|extra| (mode, extra)))
    {
        let before = endpoint.requests.lock().unwrap().len();
        endpoint.enqueue_mode(Some(&selector), true, mode);
        let rejected = run_cli(home.path(), cwd.path(), &endpoint.url, &extra);
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stdout).contains("continuation_parent_mismatch"),
            "wrong session must fail ownership validation: {rejected:?}"
        );
        assert_eq!(
            endpoint.requests.lock().unwrap().len(),
            before + 1,
            "a rejected resume must not invoke the child model"
        );
        assert_eq!(
            fs::read(&record_path).unwrap(),
            settled,
            "rejection must not mutate the original continuation"
        );
    }
    endpoint.finish();
}
