//! Starts the Node process a script runs in and speaks its protocol: tool
//! calls come out as lines, answers go back as lines, and the last line says
//! how the script ended.

use std::env;
use std::fs;
use std::io::{self, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use orca_core::capability::CapabilitySet;
use orca_core::execution_broker::{ExecutionBroker, LaunchError};

use crate::workflow::host::{
    WorkflowHost, WorkflowHostChild, WorkflowHostFileGuard, node_confinement_args, node_entry_path,
    read_bounded_workflow_frame, spawn_retained_reader,
};

const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(20);
const RETAINED_STDERR_BYTES: usize = 4 * 1024;

/// What stops a script that the script itself does not end.
#[derive(Clone, Copy, Debug)]
pub struct ScriptHostLimits {
    /// How long the script may run, tool calls included.
    pub wall_clock: Duration,
    /// The longest line the host may send: one tool call or the result.
    pub max_line_bytes: usize,
}

/// One tool call a script made.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptToolCall {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

/// How a script ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptHostOutcome {
    Completed { output: String },
    Failed { error: String },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostLine {
    ToolCall {
        call_id: String,
        tool: String,
        #[serde(default)]
        arguments: Value,
    },
    Completed {
        output: String,
    },
    Failed {
        error: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostAnswer<'a> {
    ToolResult { call_id: &'a str, output: &'a str },
    ToolError { call_id: &'a str, error: &'a str },
}

enum HostRead {
    Line(Vec<u8>),
    Ended,
    Unreadable,
}

pub struct ScriptHost;

impl ScriptHost {
    /// Runs `script` to its end. Each tool call it makes is answered by
    /// `on_tool_call`: `Ok(text)` is what the call resolves to, `Err(text)`
    /// what it rejects with. A script stopped by `should_cancel` is an
    /// `Interrupted` error, not an outcome.
    pub fn run(
        script: &str,
        limits: ScriptHostLimits,
        on_tool_call: &mut dyn FnMut(ScriptToolCall) -> Result<String, String>,
        should_cancel: &dyn Fn() -> bool,
    ) -> io::Result<ScriptHostOutcome> {
        let host_path = temp_file("host", "mjs", include_str!("host.mjs"))?;
        let _host_file = WorkflowHostFileGuard::new(host_path.clone());
        let script_path = temp_file("script", "js", script)?;
        let _script_file = WorkflowHostFileGuard::new(script_path.clone());

        let node = WorkflowHost::node_executable();
        let mut command = Command::new(&node);
        command
            .args(node_confinement_args(&node, &host_path, &script_path, None))
            .arg(&host_path)
            .arg(&script_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        orca_platform::process::remove_credential_env(&mut command);
        #[cfg(unix)]
        command.process_group(0);

        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let broker = ExecutionBroker::with_backend(
            orca_core::capability::EnforcementState::Advisory,
            "script-user-trusted",
        );
        let launched = broker
            .launch_user_trusted(command, "script-host", cwd, CapabilitySet::read_only())
            .map_err(|error| match error {
                LaunchError::Spawn(error) => error,
                other => io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("script host broker rejected launch: {other:?}"),
                ),
            })?;
        // Dropping this stops the process and anything it started, so every
        // return below leaves nothing running.
        let mut child = WorkflowHostChild::new(launched.child, launched.process_job);
        let mut stdin = take(child.child_mut()?.stdin.take(), "stdin")?;
        let stdout = take(child.child_mut()?.stdout.take(), "stdout")?;
        let stderr = take(child.child_mut()?.stderr.take(), "stderr")?;
        let stderr = spawn_retained_reader(stderr, RETAINED_STDERR_BYTES, "stderr");

        let (sender, lines) = mpsc::sync_channel(1);
        let max_line_bytes = limits.max_line_bytes;
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let read = match read_bounded_workflow_frame(&mut reader, max_line_bytes) {
                    Ok(Some(line)) => HostRead::Line(line),
                    Ok(None) => HostRead::Ended,
                    Err(_) => HostRead::Unreadable,
                };
                let last = !matches!(read, HostRead::Line(_));
                if sender.send(read).is_err() || last {
                    return;
                }
            }
        });

        let deadline = Instant::now() + limits.wall_clock;
        loop {
            if should_cancel() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "the script was cancelled",
                ));
            }
            if Instant::now() >= deadline {
                return Ok(ScriptHostOutcome::Failed {
                    error: format!(
                        "the script ran longer than {} and was stopped",
                        seconds(limits.wall_clock)
                    ),
                });
            }
            let line = match lines.recv_timeout(CONTROL_POLL_INTERVAL) {
                Ok(HostRead::Line(line)) => line,
                Ok(HostRead::Unreadable) => {
                    return Ok(ScriptHostOutcome::Failed {
                        error: format!(
                            "the script sent more than {} bytes in one tool call or result and was stopped",
                            limits.max_line_bytes
                        ),
                    });
                }
                Ok(HostRead::Ended) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.force_terminate_and_wait();
                    let stderr = stderr
                        .join()
                        .ok()
                        .and_then(Result::ok)
                        .map(|stderr| String::from_utf8_lossy(&stderr.bytes).trim().to_string())
                        .unwrap_or_default();
                    return Ok(ScriptHostOutcome::Failed {
                        error: if stderr.is_empty() {
                            "the script host stopped before the script finished".to_string()
                        } else {
                            format!("the script host stopped before the script finished:\n{stderr}")
                        },
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            };

            match serde_json::from_slice::<HostLine>(&line) {
                Ok(HostLine::ToolCall {
                    call_id,
                    tool,
                    arguments,
                }) => {
                    let answer = on_tool_call(ScriptToolCall {
                        call_id: call_id.clone(),
                        tool,
                        arguments,
                    });
                    let answer = match &answer {
                        Ok(output) => HostAnswer::ToolResult {
                            call_id: &call_id,
                            output,
                        },
                        Err(error) => HostAnswer::ToolError {
                            call_id: &call_id,
                            error,
                        },
                    };
                    let mut line = serde_json::to_vec(&answer).map_err(io::Error::other)?;
                    line.push(b'\n');
                    // A host that has gone will not read this; the next read
                    // says how it ended.
                    let _ = stdin.write_all(&line).and_then(|()| stdin.flush());
                }
                Ok(HostLine::Completed { output }) => {
                    return Ok(ScriptHostOutcome::Completed { output });
                }
                Ok(HostLine::Failed { error }) => return Ok(ScriptHostOutcome::Failed { error }),
                Err(_) => {
                    return Ok(ScriptHostOutcome::Failed {
                        error: "the script host sent a line that is not part of its protocol"
                            .to_string(),
                    });
                }
            }
        }
    }
}

fn take<T>(pipe: Option<T>, name: &str) -> io::Result<T> {
    pipe.ok_or_else(|| io::Error::other(format!("failed to capture script host {name}")))
}

/// Writes `contents` to a file of this process's own in the temp directory and
/// returns its path as Node can load it.
fn temp_file(kind: &str, extension: &str, contents: &str) -> io::Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let path = env::temp_dir().join(format!(
        "orca-script-{kind}-{}-{}.{extension}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, contents)?;
    Ok(node_entry_path(path))
}

fn seconds(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}
