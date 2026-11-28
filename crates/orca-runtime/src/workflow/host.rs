use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use orca_core::capability::CapabilitySet;
use orca_core::execution_broker::{ExecutionBroker, LaunchError};
use orca_core::redaction::redact_sensitive_text;
use orca_core::retained_output::{RetainedOutput, RetainedOutputSnapshot};
use orca_platform::fs::{AtomicWritePolicy, atomic_write};
use orca_platform::process::ProcessJob;

use super::ipc::WorkflowIpcContext;

const WORKFLOW_HOST_MODULE: &[u8] = include_bytes!("host.mjs");
const WORKFLOW_HOST_BOOTSTRAP: &str = include_str!("host_bootstrap.mjs");

const MAX_WORKFLOW_HOST_FRAME_BYTES: usize = 1024 * 1024;
const MAX_WORKFLOW_HOST_EVENTS: usize = 8_192;
const MAX_WORKFLOW_HOST_EVENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_WORKFLOW_HOST_STDERR_BYTES: usize = 64 * 1024;
const MAX_WORKFLOW_HOST_STARTUP_STDERR_BYTES: usize = 2 * 1024;
const WORKFLOW_HOST_FRAME_CHANNEL_CAPACITY: usize = 8;
const DEFAULT_WORKFLOW_HOST_AGENT_WORKERS: usize = 16;
const WORKFLOW_HOST_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const WORKFLOW_HOST_EXIT_GRACE: Duration = Duration::from_secs(2);
const NODE_PERMISSION_PROBE_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostEvent {
    PhaseStarted {
        name: String,
    },
    PhaseCompleted {
        name: String,
    },
    PhaseFailed {
        name: String,
        error: String,
        #[serde(default)]
        fallback: Option<String>,
    },
    AgentCall {
        call_id: String,
        call_path: String,
        phase: Option<String>,
        prompt: String,
        opts: Value,
    },
    WorkflowCompleted {
        result: Value,
    },
    WorkflowFailed {
        error: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostCommand {
    AgentResult { call_id: String, result: Value },
    AgentError { call_id: String, error: String },
}

#[derive(Clone, Debug, Default)]
pub struct WorkflowHost;

#[derive(Clone, Debug)]
pub struct WorkflowHostIpcPaths {
    pub mailbox_path: PathBuf,
    pub task_lists_path: PathBuf,
}

#[derive(Clone, Copy, Debug)]
struct WorkflowHostRunPolicy<'a> {
    collect_events: bool,
    max_agent_workers: usize,
    workflow_ipc: Option<&'a WorkflowIpcContext>,
    script: Option<(&'a str, &'a str)>,
}

impl<'a> WorkflowHostRunPolicy<'a> {
    fn collecting() -> Self {
        Self {
            collect_events: true,
            max_agent_workers: DEFAULT_WORKFLOW_HOST_AGENT_WORKERS,
            workflow_ipc: None,
            script: None,
        }
    }

    fn callback_only(
        max_agent_workers: usize,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        script: Option<(&'a str, &'a str)>,
    ) -> Self {
        Self {
            collect_events: false,
            max_agent_workers: max_agent_workers.max(1),
            workflow_ipc,
            script,
        }
    }
}

impl WorkflowHost {
    pub fn node_executable() -> PathBuf {
        node_command()
    }

    pub fn node_available() -> bool {
        let mut command = Command::new(Self::node_executable());
        command
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let broker = ExecutionBroker::with_backend(
            orca_core::capability::EnforcementState::Advisory,
            "workflow-node-probe",
        );
        broker
            .launch_user_trusted(
                command,
                "workflow-node-probe",
                cwd,
                CapabilitySet::read_only(),
            )
            .and_then(|mut launched| {
                launched
                    .child
                    .wait()
                    .map_err(LaunchError::Spawn)
                    .map_err(|_| LaunchError::EnforcementAdvisory)
            })
            .map(|status| status.success())
            .unwrap_or(false)
    }

    pub fn run_collecting_events(script_path: &Path, args: Value) -> io::Result<Vec<HostEvent>> {
        Self::run_collecting_events_with_agent(script_path, args, |call| {
            Ok(HostCommand::AgentResult {
                call_id: call.call_id.clone(),
                result: serde_json::json!({
                    "callId": call.call_id,
                    "prompt": call.prompt,
                    "cached": false,
                }),
            })
        })
    }

    pub fn run_collecting_events_with_ipc_paths(
        script_path: &Path,
        args: Value,
        ipc_paths: &WorkflowHostIpcPaths,
    ) -> io::Result<Vec<HostEvent>> {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            Command::new(Self::node_executable()),
            script_path,
            args,
            Some(ipc_paths),
            WorkflowHostRunPolicy::collecting(),
            |call| {
                Ok(HostCommand::AgentResult {
                    call_id: call.call_id.clone(),
                    result: serde_json::json!({
                        "callId": call.call_id,
                        "prompt": call.prompt,
                        "cached": false,
                    }),
                })
            },
            |_| Ok(()),
            || Ok(false),
            || {},
        )
    }

    pub fn run_collecting_events_with_agent<F>(
        script_path: &Path,
        args: Value,
        on_agent_call: F,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
    {
        Self::run_collecting_events_with_agent_and_event_callback(
            script_path,
            args,
            on_agent_call,
            |_| Ok(()),
        )
    }

    pub fn run_collecting_events_with_agent_and_event_callback<F, E>(
        script_path: &Path,
        args: Value,
        on_agent_call: F,
        on_event: E,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
    {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            Command::new(Self::node_executable()),
            script_path,
            args,
            None,
            WorkflowHostRunPolicy::collecting(),
            on_agent_call,
            on_event,
            || Ok(false),
            || {},
        )
    }

    pub fn run_collecting_events_with_agent_and_event_callback_with_ipc_paths<F, E>(
        script_path: &Path,
        args: Value,
        ipc_paths: &WorkflowHostIpcPaths,
        on_agent_call: F,
        on_event: E,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
    {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            Command::new(Self::node_executable()),
            script_path,
            args,
            Some(ipc_paths),
            WorkflowHostRunPolicy::collecting(),
            on_agent_call,
            on_event,
            || Ok(false),
            || {},
        )
    }

    pub(crate) fn run_with_agent_event_control_and_ipc_paths<F, E, C, A>(
        (script_path, script, script_digest): (&Path, &str, &str),
        args: Value,
        (ipc_paths, workflow_ipc): (&WorkflowHostIpcPaths, &WorkflowIpcContext),
        max_agent_workers: usize,
        on_agent_call: F,
        on_event: E,
        should_cancel: C,
        on_abort: A,
    ) -> io::Result<()>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
        C: Fn() -> io::Result<bool>,
        A: Fn(),
    {
        Self::run_collecting_events_with_agent_and_event_callback_inner(
            Command::new(Self::node_executable()),
            script_path,
            args,
            Some(ipc_paths),
            WorkflowHostRunPolicy::callback_only(
                max_agent_workers,
                Some(workflow_ipc),
                Some((script, script_digest)),
            ),
            on_agent_call,
            on_event,
            should_cancel,
            on_abort,
        )
        .map(|_| ())
    }

    fn run_collecting_events_with_agent_and_event_callback_inner<F, E, C, A>(
        mut command: Command,
        script_path: &Path,
        args: Value,
        ipc_paths: Option<&WorkflowHostIpcPaths>,
        policy: WorkflowHostRunPolicy<'_>,
        on_agent_call: F,
        mut on_event: E,
        should_cancel: C,
        on_abort: A,
    ) -> io::Result<Vec<HostEvent>>
    where
        F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
        E: FnMut(&HostEvent) -> io::Result<()>,
        C: Fn() -> io::Result<bool>,
        A: Fn(),
    {
        let script_file;
        let (script, digest) = match policy.script {
            Some((script, digest)) => (script.as_bytes(), digest),
            None => {
                script_file = fs::read(script_path)?;
                (script_file.as_slice(), "")
            }
        };
        let replies = tempfile::Builder::new()
            .prefix("orca-workflow-replies-")
            .tempdir()?;
        let task_list_reply = node_entry_path(replies.path().to_path_buf()).join("task-list.json");
        let workflow_ipc = match policy.workflow_ipc {
            Some(ipc) => ipc.clone(),
            None => match ipc_paths {
                Some(paths) => {
                    WorkflowIpcContext::new_durable_task_lists(paths.task_lists_path.clone())?
                }
                None => WorkflowIpcContext::new(),
            },
        };
        let args_json = serialize_bounded_json(
            &args,
            MAX_WORKFLOW_HOST_FRAME_BYTES,
            "workflow host arguments",
        )?;
        let args_json = String::from_utf8(args_json)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

        let node = PathBuf::from(command.get_program());
        #[cfg(test)]
        let preload = command
            .get_args()
            .collect::<Vec<_>>()
            .windows(2)
            .find(|args| args[0] == "--require")
            .map(|args| PathBuf::from(args[1]));
        #[cfg(test)]
        let read_paths = preload.as_deref().into_iter().collect::<Vec<_>>();
        #[cfg(not(test))]
        let read_paths = [];
        command.args(node_confinement_args(
            &node,
            &read_paths,
            ipc_paths,
            Some(&task_list_reply),
        ));
        orca_platform::process::remove_credential_env(&mut command);
        workflow_host_bootstrap(&mut command);
        command.arg(script_path).arg(args_json);
        let mailbox_path = ipc_paths.map(|paths| paths.mailbox_path.as_path());
        if let Some(path) = mailbox_path {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
        }
        command
            .arg(mailbox_path.unwrap_or_else(|| Path::new("")))
            .arg(&task_list_reply)
            .arg(MAX_WORKFLOW_HOST_FRAME_BYTES.to_string())
            .arg(digest)
            .arg(WORKFLOW_HOST_MODULE.len().to_string())
            .arg(super::script::sha256_hex(WORKFLOW_HOST_MODULE))
            .arg(script.len().to_string());
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let broker = ExecutionBroker::with_backend(
            orca_core::capability::EnforcementState::Advisory,
            "workflow-user-trusted",
        );
        let launched = broker
            .launch_user_trusted(command, "workflow-host", cwd, CapabilitySet::read_only())
            .map_err(|error| match error {
                LaunchError::Spawn(error) => error,
                other => io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("workflow host broker rejected launch: {other:?}"),
                ),
            })?;
        let (child, process_job) = (launched.child, launched.process_job);
        let mut child = WorkflowHostChild::new(child, process_job);
        let stdin = child
            .child_mut()?
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stdin"))?;
        let stdin = Arc::new(Mutex::new(stdin));
        let stdout = child
            .child_mut()?
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stdout"))?;
        let stderr = child
            .child_mut()?
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("failed to capture workflow host stderr"))?;

        let (frame_tx, frame_rx) = mpsc::sync_channel(WORKFLOW_HOST_FRAME_CHANNEL_CAPACITY);
        let stdout_handle = spawn_workflow_frame_reader(BufReader::new(stdout), frame_tx);
        let stderr_handle = spawn_retained_reader(stderr, MAX_WORKFLOW_HOST_STDERR_BYTES, "stderr");

        let mut events = Vec::new();
        let mut workflow_failed = None;
        let agent_error = Arc::new(Mutex::new(None));
        let fatal_worker_error = Arc::new(Mutex::new(None));
        let abort_workers = Arc::new(AtomicBool::new(false));
        let mut stdout_closed_early = false;
        let on_agent_call = &on_agent_call;
        let execution_result = thread::scope(|scope| -> io::Result<()> {
            // Start the input writer alongside the output readers. A host
            // that exits or stops reading cannot hold up cancellation.
            let input = Arc::clone(&stdin);
            let (locked, ready) = mpsc::sync_channel(0);
            let (written, startup_finished) = mpsc::sync_channel(1);
            scope.spawn(move || {
                if let Ok(mut input) = input.lock() {
                    let _ = locked.send(());
                    let _ = input
                        .write_all(WORKFLOW_HOST_MODULE)
                        .and_then(|()| input.write_all(script));
                }
                let _ = written.send(());
            });
            ready
                .recv()
                .map_err(|_| io::Error::other("workflow host stdin lock poisoned"))?;
            let worker_count = policy.max_agent_workers.max(1);
            // The frame count and byte limits bound queued calls. Keep reading
            // frames while workers wait for the script to receive their results,
            // since a synchronous task list call may be blocking the script.
            let (call_tx, call_rx) =
                crossbeam_channel::bounded::<AgentCall>(MAX_WORKFLOW_HOST_EVENTS);
            let mut worker_handles = Vec::with_capacity(worker_count);
            for _ in 0..worker_count {
                let receiver = call_rx.clone();
                let writer = Arc::clone(&stdin);
                let error_slot = Arc::clone(&agent_error);
                let fatal_error_slot = Arc::clone(&fatal_worker_error);
                let abort = Arc::clone(&abort_workers);
                worker_handles.push(scope.spawn(move || {
                    run_workflow_agent_worker(
                        receiver,
                        writer,
                        error_slot,
                        fatal_error_slot,
                        abort,
                        on_agent_call,
                    );
                }));
            }

            let run_result = (|| -> io::Result<()> {
                let mut event_count = 0usize;
                let mut event_bytes = 0usize;
                loop {
                    if should_cancel()? {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "workflow host cancelled",
                        ));
                    }
                    if let Some(error) = read_recorded_error(&fatal_worker_error)? {
                        return Err(io::Error::other(error));
                    }

                    let frame = match frame_rx.recv_timeout(WORKFLOW_HOST_CONTROL_POLL_INTERVAL) {
                        Ok(WorkflowHostFrame::Data(frame)) => frame,
                        Ok(WorkflowHostFrame::Eof) => {
                            stdout_closed_early = true;
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "workflow host stdout closed before a terminal event",
                            ));
                        }
                        Ok(WorkflowHostFrame::Error(error)) => return Err(error),
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "workflow host stdout reader disconnected",
                            ));
                        }
                    };

                    if frame.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    event_count = event_count.saturating_add(1);
                    event_bytes = event_bytes.saturating_add(frame.len().saturating_add(1));
                    if event_count > MAX_WORKFLOW_HOST_EVENTS {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "workflow host event count exceeded {MAX_WORKFLOW_HOST_EVENTS}"
                            ),
                        ));
                    }
                    if event_bytes > MAX_WORKFLOW_HOST_EVENT_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "workflow host event bytes exceeded {MAX_WORKFLOW_HOST_EVENT_BYTES}"
                            ),
                        ));
                    }

                    let message: WorkflowHostMessage = serde_json::from_slice(&frame)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    let event = match message {
                        WorkflowHostMessage::Event(event) => event,
                        WorkflowHostMessage::TaskList { task_list_call } => {
                            task_list_call.reply(&workflow_ipc, &task_list_reply)?;
                            continue;
                        }
                    };
                    if let HostEvent::WorkflowFailed { error } = &event {
                        workflow_failed = Some(error.clone());
                    }
                    let is_terminal = matches!(
                        event,
                        HostEvent::WorkflowCompleted { .. } | HostEvent::WorkflowFailed { .. }
                    );
                    on_event(&event)?;
                    if let HostEvent::AgentCall {
                        call_id,
                        call_path,
                        phase,
                        prompt,
                        opts,
                    } = &event
                    {
                        call_tx
                            .try_send(AgentCall {
                                call_id: call_id.clone(),
                                call_path: call_path.clone(),
                                phase: phase.clone(),
                                prompt: prompt.clone(),
                                opts: opts.clone(),
                            })
                            .map_err(|error| {
                                io::Error::other(format!(
                                    "workflow host agent worker queue: {error}"
                                ))
                            })?;
                    }
                    if policy.collect_events {
                        events.push(event);
                    }
                    if is_terminal {
                        on_abort();
                        abort_workers.store(true, Ordering::Release);
                        if startup_finished
                            .recv_timeout(WORKFLOW_HOST_EXIT_GRACE)
                            .is_err()
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "workflow host did not finish reading its startup input",
                            ));
                        }
                        return Ok(());
                    }
                }
            })();

            if run_result.is_err() {
                on_abort();
                abort_workers.store(true, Ordering::Release);
                let _ = child.terminate_and_wait();
            }
            drop(call_tx);

            let mut worker_panic = false;
            for handle in worker_handles {
                if handle.join().is_err() {
                    worker_panic = true;
                }
            }
            if worker_panic && run_result.is_ok() {
                on_abort();
                abort_workers.store(true, Ordering::Release);
                let _ = child.terminate_and_wait();
                return Err(io::Error::other("workflow host agent worker panicked"));
            }
            if run_result.is_ok()
                && let Some(error) = read_recorded_error(&fatal_worker_error)?
            {
                on_abort();
                abort_workers.store(true, Ordering::Release);
                let _ = child.terminate_and_wait();
                return Err(io::Error::other(error));
            }
            run_result
        });
        drop(stdin);

        let exit_result = if execution_result.is_ok() {
            child.wait_for_exit(WORKFLOW_HOST_EXIT_GRACE)
        } else {
            child.cached_exit().map(|status| WorkflowHostExit {
                status,
                forced: true,
            })
        };
        drop(frame_rx);
        let stdout_result = join_host_thread(stdout_handle, "stdout frame reader");
        let stderr_result = join_host_thread(stderr_handle, "stderr reader");

        if let Err(error) = execution_result {
            if stdout_closed_early && let Ok(stderr) = stderr_result {
                let start = if stderr.is_truncated() {
                    stderr.retained_head_bytes()
                } else {
                    0
                };
                let stderr =
                    redact_sensitive_text(String::from_utf8_lossy(&stderr.bytes[start..]).trim());
                if !stderr.is_empty() {
                    let start = stderr.ceil_char_boundary(
                        stderr
                            .len()
                            .saturating_sub(MAX_WORKFLOW_HOST_STARTUP_STDERR_BYTES),
                    );
                    return Err(io::Error::new(
                        error.kind(),
                        format!("{error}: {}", &stderr[start..]),
                    ));
                }
            }
            return Err(error);
        }
        stdout_result?;
        let stderr = stderr_result?;
        let exit = exit_result?;
        if exit.forced {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "workflow host did not exit after its terminal event",
            ));
        }

        if let Some(error) = read_recorded_error(&agent_error)? {
            return Err(io::Error::other(error));
        }
        if !exit.status.success() {
            if workflow_failed.is_some() {
                return Ok(events);
            }

            let stderr = format_stderr(stderr);
            let message = if stderr.is_empty() {
                format!("workflow host exited with status {}", exit.status)
            } else {
                format!("workflow host exited with status {}: {stderr}", exit.status)
            };
            return Err(io::Error::other(message));
        }

        Ok(events)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WorkflowHostMessage {
    Event(HostEvent),
    TaskList {
        task_list_call: WorkflowHostTaskListCall,
    },
}

#[derive(Deserialize)]
struct WorkflowHostTaskListCall {
    id: u64,
    request: WorkflowHostTaskListRequest,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum WorkflowHostTaskListRequest {
    Create {
        name: String,
        items: Vec<Value>,
    },
    Claim {
        name: String,
        by: String,
    },
    Complete {
        name: String,
        task_id: String,
        result: Value,
        by: String,
    },
    List {
        name: String,
    },
}

impl WorkflowHostTaskListCall {
    fn reply(self, ipc: &WorkflowIpcContext, reply_path: &Path) -> io::Result<()> {
        use WorkflowHostTaskListRequest::*;

        let result = match self.request {
            Create { name, items } => ipc.create_task_list(&name, items),
            Claim { name, by } => ipc.claim_task(&name, Some(&by)),
            Complete {
                name,
                task_id,
                result,
                by,
            } => ipc.complete_task(&name, &task_id, result, Some(&by)),
            List { name } => ipc.list_tasks(&name),
        };
        let reply = match result {
            Ok(value) => serde_json::json!({ "id": self.id, "value": value }),
            Err(error) => serde_json::json!({ "id": self.id, "error": error }),
        };
        atomic_write(
            reply_path,
            &serde_json::to_vec(&reply)?,
            AtomicWritePolicy::NoFollow,
        )
        .map_err(io::Error::other)
    }
}

enum WorkflowHostFrame {
    Data(Vec<u8>),
    Eof,
    Error(io::Error),
}

struct WorkflowHostExit {
    status: ExitStatus,
    forced: bool,
}

struct WorkflowHostChild {
    child: Option<Child>,
    process_job: ProcessJob,
    exit_status: Option<ExitStatus>,
}

impl WorkflowHostChild {
    fn new(child: Child, process_job: ProcessJob) -> Self {
        Self {
            child: Some(child),
            process_job,
            exit_status: None,
        }
    }

    fn child_mut(&mut self) -> io::Result<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| io::Error::other("workflow host child already reaped"))
    }

    fn cached_exit(&self) -> io::Result<ExitStatus> {
        self.exit_status
            .ok_or_else(|| io::Error::other("workflow host child has no exit status"))
    }

    fn terminate_and_wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.exit_status {
            return Ok(status);
        }
        match self.child_mut()?.try_wait() {
            Ok(Some(status)) => {
                self.exit_status = Some(status);
                self.terminate_process_group();
                return Ok(status);
            }
            Ok(None) | Err(_) => {}
        }
        self.force_terminate_and_wait()
    }

    fn force_terminate_and_wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.exit_status {
            return Ok(status);
        }
        let _ = self.process_job.terminate(137);
        let child = self.child_mut()?;
        orca_tools::process::kill_child_tree(child);
        let status = child.wait()?;
        self.exit_status = Some(status);
        Ok(status)
    }

    fn terminate_process_group(&mut self) {
        let _ = self.process_job.terminate(137);
        if let Some(child) = self.child.as_mut() {
            orca_tools::process::kill_child_tree(child);
        }
    }

    fn wait_for_exit(&mut self, grace: Duration) -> io::Result<WorkflowHostExit> {
        if let Some(status) = self.exit_status {
            return Ok(WorkflowHostExit {
                status,
                forced: false,
            });
        }
        let deadline = Instant::now()
            .checked_add(grace)
            .unwrap_or_else(Instant::now);
        loop {
            match self.child_mut()?.try_wait() {
                Ok(Some(status)) => {
                    self.exit_status = Some(status);
                    self.terminate_process_group();
                    return Ok(WorkflowHostExit {
                        status,
                        forced: false,
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = self.force_terminate_and_wait();
                    return Err(error);
                }
            }
            if Instant::now() >= deadline {
                let status = self.terminate_and_wait()?;
                return Ok(WorkflowHostExit {
                    status,
                    forced: true,
                });
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for WorkflowHostChild {
    fn drop(&mut self) {
        if self.exit_status.is_none() {
            let _ = self.terminate_and_wait();
        }
    }
}

fn spawn_workflow_frame_reader<R>(
    mut reader: R,
    sender: mpsc::SyncSender<WorkflowHostFrame>,
) -> thread::JoinHandle<io::Result<()>>
where
    R: BufRead + Send + 'static,
{
    thread::spawn(move || {
        loop {
            match read_bounded_workflow_frame(&mut reader, MAX_WORKFLOW_HOST_FRAME_BYTES) {
                Ok(Some(frame)) => {
                    if sender.send(WorkflowHostFrame::Data(frame)).is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => {
                    let _ = sender.send(WorkflowHostFrame::Eof);
                    return Ok(());
                }
                Err(error) => {
                    let _ = sender.send(WorkflowHostFrame::Error(error));
                    return Ok(());
                }
            }
        }
    })
}

fn read_bounded_workflow_frame<R: BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::with_capacity(max_bytes.min(8 * 1024));
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow host stdout ended with a partial frame",
            ));
        }

        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let consumed = newline + 1;
            if frame.len().saturating_add(consumed) > max_bytes {
                return Err(workflow_frame_too_large(max_bytes));
            }
            frame.extend_from_slice(&available[..newline]);
            reader.consume(consumed);
            return Ok(Some(frame));
        }

        if frame.len().saturating_add(available.len()) > max_bytes {
            return Err(workflow_frame_too_large(max_bytes));
        }
        let consumed = available.len();
        frame.extend_from_slice(available);
        reader.consume(consumed);
    }
}

fn workflow_frame_too_large(max_bytes: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("workflow host frame exceeded {max_bytes} bytes"),
    )
}

fn spawn_retained_reader<R>(
    mut reader: R,
    retained_bytes: usize,
    stream: &'static str,
) -> thread::JoinHandle<io::Result<RetainedOutputSnapshot>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut output = RetainedOutput::new(retained_bytes);
        let mut chunk = [0_u8; 8 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(output.into_snapshot()),
                Ok(read) => output.append(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("workflow host {stream} read failed: {error}"),
                    ));
                }
            }
        }
    })
}

fn join_host_thread<T>(handle: thread::JoinHandle<io::Result<T>>, label: &str) -> io::Result<T> {
    handle
        .join()
        .map_err(|_| io::Error::other(format!("workflow host {label} panicked")))?
}

fn run_workflow_agent_worker<F>(
    receiver: crossbeam_channel::Receiver<AgentCall>,
    writer: Arc<Mutex<impl Write>>,
    error_slot: Arc<Mutex<Option<String>>>,
    fatal_error_slot: Arc<Mutex<Option<String>>>,
    abort: Arc<AtomicBool>,
    on_agent_call: &F,
) where
    F: Fn(AgentCall) -> io::Result<HostCommand> + Send + Sync,
{
    loop {
        let received = receiver.recv();
        let Ok(call) = received else {
            return;
        };
        if abort.load(Ordering::Acquire) {
            return;
        }

        let call_id = call.call_id.clone();
        let command =
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_agent_call(call))) {
                Err(_) => {
                    record_first_agent_error(
                        &fatal_error_slot,
                        "workflow host agent callback panicked".to_string(),
                    );
                    return;
                }
                Ok(Ok(command)) => command,
                Ok(Err(error)) => {
                    record_first_agent_error(&error_slot, error.to_string());
                    HostCommand::AgentError {
                        call_id,
                        error: "workflow host failed to answer agent call".to_string(),
                    }
                }
            };
        if abort.load(Ordering::Acquire) {
            continue;
        }
        if let Err(error) = write_host_command(&writer, &command) {
            record_first_agent_error(&fatal_error_slot, error.to_string());
            return;
        }
    }
}

fn read_recorded_error(error_slot: &Arc<Mutex<Option<String>>>) -> io::Result<Option<String>> {
    error_slot
        .lock()
        .map(|error| error.clone())
        .map_err(|_| io::Error::other("workflow host error lock poisoned"))
}

fn format_stderr(stderr: RetainedOutputSnapshot) -> String {
    let mut message = String::from_utf8_lossy(&stderr.bytes).trim().to_string();
    if stderr.omitted_bytes > 0 {
        message.push_str(&format!(
            "\n[{} workflow stderr bytes omitted]",
            stderr.omitted_bytes
        ));
    }
    message
}

fn write_host_command(writer: &Arc<Mutex<impl Write>>, command: &HostCommand) -> io::Result<()> {
    let command_json = serialize_bounded_json(
        command,
        MAX_WORKFLOW_HOST_FRAME_BYTES.saturating_sub(1),
        "workflow host command frame",
    )?;
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("workflow host stdin lock poisoned"))?;
    writer.write_all(&command_json)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    label: &'static str,
}

impl BoundedJsonWriter {
    fn new(max_bytes: usize, label: &'static str) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(8 * 1024)),
            max_bytes,
            label,
        }
    }
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(buffer.len()) > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} exceeded {} bytes", self.label, self.max_bytes),
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_bounded_json<T: Serialize>(
    value: &T,
    max_bytes: usize,
    label: &'static str,
) -> io::Result<Vec<u8>> {
    let mut writer = BoundedJsonWriter::new(max_bytes, label);
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(writer.bytes)
}

fn record_first_agent_error(error_slot: &Arc<Mutex<Option<String>>>, error: String) {
    if let Ok(mut slot) = error_slot.lock() {
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCall {
    pub call_id: String,
    pub call_path: String,
    pub phase: Option<String>,
    pub prompt: String,
    pub opts: Value,
}

fn workflow_host_bootstrap(command: &mut Command) {
    command.args([
        "--input-type=module",
        "--eval",
        WORKFLOW_HOST_BOOTSTRAP,
        "--",
        "orca-workflow-host.mjs",
    ]);
}

/// `path` as Node can load it as its entry module under the permission model.
/// Node resolves the links in an entry module's path, and needs read access
/// to each one it follows; a path with no links needs access to the file only.
fn node_entry_path(path: PathBuf) -> PathBuf {
    if cfg!(unix) {
        fs::canonicalize(&path).unwrap_or(path)
    } else {
        path
    }
}

/// The Node flags the workflow host is started with. A workflow script runs in
/// a realm of its own inside the host and is given nothing of the host's; these
/// flags are for the script that gets something anyway.
///
/// - The host's own realm cannot compile code from text, so an object of that
///   realm in a script's hands does not become code running there.
/// - Where Node has a permission model, the process reads its mailbox and task
///   list replies and can only write the mailbox. Code arrives over stdin.
///
/// A Node without a permission model is given the first flag only.
fn node_confinement_args(
    node: &Path,
    read_paths: &[&Path],
    ipc_paths: Option<&WorkflowHostIpcPaths>,
    task_list_reply: Option<&Path>,
) -> Vec<OsString> {
    let mut args = vec![OsString::from("--disallow-code-generation-from-strings")];
    if node_has_permission_model(node) {
        args.extend(permission_model_args(
            read_paths,
            ipc_paths,
            task_list_reply,
        ));
    }
    args
}

/// Allow the host to read its IPC files and replace its mailbox.
fn permission_model_args(
    read_paths: &[&Path],
    ipc_paths: Option<&WorkflowHostIpcPaths>,
    task_list_reply: Option<&Path>,
) -> Vec<OsString> {
    let allow = |access: &str, path: &Path| {
        let mut flag = OsString::from(format!("--allow-fs-{access}="));
        flag.push(path);
        flag
    };
    let mut args = vec![OsString::from("--permission")];
    args.extend(read_paths.iter().map(|path| allow("read", path)));
    if let Some(ipc_paths) = ipc_paths {
        // Node asks for read access to a file it renames.
        for path in [
            ipc_paths.mailbox_path.clone(),
            host_replacement_path(&ipc_paths.mailbox_path),
        ] {
            args.push(allow("read", &path));
            args.push(allow("write", &path));
        }
    }
    if let Some(path) = task_list_reply {
        args.push(allow("read", path));
    }
    args
}

/// Where the host writes an IPC file's next contents before renaming them into
/// place. `host.mjs` builds the same name.
fn host_replacement_path(path: &Path) -> PathBuf {
    let mut replacement = path.as_os_str().to_os_string();
    replacement.push(".host-tmp");
    PathBuf::from(replacement)
}

/// Whether `node` can be started with `--permission`. Asked of each executable
/// until it answers: an older Node refuses to start when given a flag it does
/// not know.
fn node_has_permission_model(node: &Path) -> bool {
    static KNOWN: Mutex<BTreeMap<PathBuf, bool>> = Mutex::new(BTreeMap::new());

    if let Some(known) = KNOWN.lock().ok().and_then(|known| known.get(node).copied()) {
        return known;
    }
    let answer = probe_node_permission_model(node);
    if let (Some(answer), Ok(mut known)) = (answer, KNOWN.lock()) {
        known.insert(node.to_path_buf(), answer);
    }
    answer.unwrap_or(false)
}

/// Starts `node` the way the workflow host is started, on a module that does
/// what the host does with its files and says whether it may start a process.
/// "No" is the permission model at work. Anything else — an older Node, a
/// platform where a permitted file still cannot be loaded or replaced — means
/// the host runs without the permission model rather than not at all. `None`
/// is a `node` that said nothing in time.
fn probe_node_permission_model(node: &Path) -> Option<bool> {
    const ANSWER: &[u8] = b"false";
    const MODULE: &str = r#"import { readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
const file = process.argv[2];
writeFileSync(`${file}.host-tmp`, "{}");
renameSync(`${file}.host-tmp`, file);
readFileSync(file);
rmSync(file);
process.stdout.write(String(process.permission.has("child")));
"#;

    let directory = tempfile::Builder::new()
        .prefix("orca-workflow-permission-probe-")
        .tempdir()
        .ok()?;
    let root = node_entry_path(directory.path().to_path_buf());
    let ipc_paths = WorkflowHostIpcPaths {
        mailbox_path: root.join("mailbox"),
        task_lists_path: root.join("task-lists"),
    };

    let mut command = Command::new(node);
    command
        .args(permission_model_args(&[], Some(&ipc_paths), None))
        .args([
            "--input-type=module",
            "--eval",
            MODULE,
            "--",
            "orca-workflow-permission-probe",
        ])
        .arg(&ipc_paths.mailbox_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let broker = ExecutionBroker::with_backend(
        orca_core::capability::EnforcementState::Advisory,
        "workflow-node-probe",
    );
    let launched = broker
        .launch_user_trusted(
            command,
            "workflow-node-permission-probe",
            cwd,
            CapabilitySet::read_only(),
        )
        .ok()?;
    // Whatever this is, it does not outlive the question: the child and
    // anything it started are stopped when `child` is dropped.
    let mut child = WorkflowHostChild::new(launched.child, launched.process_job);
    let stdout = child.child_mut().ok()?.stdout.take()?;

    // A Node with the permission model prints the answer and exits. Reading a
    // little past it tells anything that prints more from Node without
    // waiting for it to finish.
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut printed = Vec::new();
        let _ = stdout
            .take(ANSWER.len() as u64 + 1)
            .read_to_end(&mut printed);
        let _ = sender.send(printed);
    });
    receiver
        .recv_timeout(NODE_PERMISSION_PROBE_DEADLINE)
        .ok()
        .map(|printed| printed == ANSWER)
}

fn node_command() -> PathBuf {
    for key in ["ORCA_NODE_PATH", "ORCA_NODE"] {
        if let Some(path) = env::var_os(key).filter(|path| !path.is_empty()) {
            return PathBuf::from(path);
        }
    }

    if let Some(path) = node_from_npm_package_root() {
        return path;
    }

    if let Some(path) = node_from_path_sibling() {
        return path;
    }

    PathBuf::from("node")
}

fn node_from_npm_package_root() -> Option<PathBuf> {
    let package_root = env::var_os("ORCA_MANAGED_PACKAGE_ROOT")?;
    let package_root = PathBuf::from(package_root);
    for candidate in [
        package_root.join("node").join("bin").join("node"),
        package_root
            .join("..")
            .join("node")
            .join("bin")
            .join("node"),
    ] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn node_from_path_sibling() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join("..").join("node").join("bin").join("node");
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct TestEnvVar {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    #[cfg(unix)]
    impl TestEnvVar {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = env::var_os(key);
            unsafe {
                env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    #[cfg(unix)]
    impl Drop for TestEnvVar {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    env::set_var(self.key, previous);
                } else {
                    env::remove_var(self.key);
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_at_the_old_host_module_name_is_left_untouched() {
        if !WorkflowHost::node_available() {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let sentinel = fixture.path().join("sentinel");
        fs::write(&sentinel, "keep this file").unwrap();
        let old = env::temp_dir().join(format!("orca-workflow-host-{}-0.mjs", std::process::id()));
        std::os::unix::fs::symlink(&sentinel, &old).unwrap();
        let script = fixture.path().join("workflow.js");
        fs::write(&script, "export const meta = {}; export default true;").unwrap();
        let result = WorkflowHost::run_collecting_events(&script, Value::Null);
        fs::remove_file(old).unwrap();
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep this file");
        assert!(
            matches!(result.unwrap().last(), Some(HostEvent::WorkflowCompleted { result }) if result == &Value::Bool(true))
        );
    }

    #[test]
    fn a_modified_host_module_is_refused() {
        if !WorkflowHost::node_available() {
            return;
        }
        let replacement = b"console.log('replacement ran')";
        let mut command = Command::new(WorkflowHost::node_executable());
        workflow_host_bootstrap(&mut command);
        command
            .args(["script.js", "null", "", "", "1024", ""])
            .arg(replacement.len().to_string())
            .arg(super::super::script::sha256_hex(WORKFLOW_HOST_MODULE))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(replacement).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("replacement ran"));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("workflow host module changed before launch")
        );
    }

    #[test]
    fn workflow_bootstrap_retries_stdin_and_delivers_agent_results() {
        if !WorkflowHost::node_available() {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let command = node_with_preload(
            fixture.path(),
            r#"
const fs = require('node:fs');
const { readSync } = fs;
let attempts = 0;
fs.readSync = (fd, buffer, offset, length, position) => {
  if (fd === 0) {
    attempts++;
    if (attempts <= 2) throw Object.assign(new Error('pipe has no data yet'), { code: 'EAGAIN' });
    if (attempts === 3) length = Math.min(length, 7);
  }
  return readSync(fd, buffer, offset, length, position);
};
require('node:module').syncBuiltinESMExports();
"#,
        );
        let script = fixture.path().join("workflow.js");
        fs::write(
            &script,
            "export const meta = { name: 'pipe', description: 'Pipe test', phases: [] };\r\n\
             export default await agent('check');\r\n",
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        let events = WorkflowHost::run_collecting_events_with_agent_and_event_callback_inner(
            command,
            &script,
            Value::Null,
            None,
            WorkflowHostRunPolicy::collecting(),
            |call| {
                Ok(HostCommand::AgentResult {
                    call_id: call.call_id,
                    result: serde_json::json!({"reply": "received ✓"}),
                })
            },
            |_| Ok(()),
            || Ok(Instant::now() >= deadline),
            || {},
        )
        .expect("read startup payload and agent reply");

        assert!(
            matches!(events.last(), Some(HostEvent::WorkflowCompleted { result })
                if *result == serde_json::json!({"reply": "received ✓"}))
        );
    }

    #[test]
    fn workflow_bootstrap_reports_eof_as_short_input() {
        if !WorkflowHost::node_available() {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let mut command = node_with_preload(
            fixture.path(),
            r#"
const fs = require('node:fs');
const { readSync } = fs;
fs.readSync = (fd, ...args) => {
  if (fd === 0) throw Object.assign(new Error('pipe closed'), { code: 'EOF' });
  return readSync(fd, ...args);
};
require('node:module').syncBuiltinESMExports();
"#,
        );
        workflow_host_bootstrap(&mut command);
        let output = command
            .args(["workflow.js", "null", "", "", "1024", "", "1"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(!output.status.success());
        assert!(
            stderr.contains("Error: workflow host input ended early"),
            "{stderr}"
        );
    }

    #[test]
    fn workflow_host_early_exit_includes_redacted_stderr_tail() {
        if !WorkflowHost::node_available() {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let command = node_with_preload(
            fixture.path(),
            r#"
const { writeFileSync } = require('node:fs');
writeFileSync(2, 'start of log\n' + '界'.repeat(23_000) + '\n');
writeFileSync(2, 'api_key=' + 'opaque-demo-value'.repeat(180) + '\n');
writeFileSync(2, 'workflow startup failed: token=opaque-test-token\n');
process.exit(7);
"#,
        );
        let script = fixture.path().join("workflow.js");
        fs::write(&script, "").unwrap();

        let error = WorkflowHost::run_collecting_events_with_agent_and_event_callback_inner(
            command,
            &script,
            Value::Null,
            None,
            WorkflowHostRunPolicy::collecting(),
            |_| unreachable!("fixture exits before sending events"),
            |_| Ok(()),
            || Ok(false),
            || {},
        )
        .expect_err("host exits before a terminal event");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        let message = error.to_string();
        let stderr = message
            .strip_prefix("workflow host stdout closed before a terminal event: ")
            .expect(&message);
        assert!(stderr.len() <= 2 * 1024);
        assert!(
            stderr.ends_with("workflow startup failed: token=<redacted>"),
            "{stderr}"
        );
        assert!(!stderr.contains("start of log"));
        assert!(!stderr.contains("opaque-demo-value"));
    }

    fn node_with_preload(directory: &Path, source: &str) -> Command {
        let preload = directory.join("preload.cjs");
        fs::write(&preload, source).unwrap();
        let mut command = Command::new(WorkflowHost::node_executable());
        command.arg("--require").arg(node_entry_path(preload));
        command
    }

    #[test]
    fn host_emitter_uses_blocking_stdout_writes() {
        let source = include_str!("host.mjs");

        assert!(source.contains("writeFileSync(1,"));
        assert!(!source.contains("process.stdout.write"));
    }

    #[test]
    #[cfg(unix)]
    fn node_available_accepts_explicit_node_path_env() {
        let _guard = crate::history::lock_test_env();
        let previous_path = env::var_os("PATH");
        let previous_node_path = env::var_os("ORCA_NODE_PATH");
        let previous_node = env::var_os("ORCA_NODE");
        let previous_package_root = env::var_os("ORCA_MANAGED_PACKAGE_ROOT");
        let temp = tempfile::tempdir().expect("tempdir");
        let node = write_fake_node(temp.path());

        unsafe {
            env::set_var("PATH", "");
            env::set_var("ORCA_NODE_PATH", &node);
            env::remove_var("ORCA_NODE");
            env::remove_var("ORCA_MANAGED_PACKAGE_ROOT");
        }

        assert!(WorkflowHost::node_available());

        unsafe {
            if let Some(previous) = previous_path {
                env::set_var("PATH", previous);
            } else {
                env::remove_var("PATH");
            }
            if let Some(previous) = previous_node_path {
                env::set_var("ORCA_NODE_PATH", previous);
            } else {
                env::remove_var("ORCA_NODE_PATH");
            }
            if let Some(previous) = previous_node {
                env::set_var("ORCA_NODE", previous);
            } else {
                env::remove_var("ORCA_NODE");
            }
            if let Some(previous) = previous_package_root {
                env::set_var("ORCA_MANAGED_PACKAGE_ROOT", previous);
            } else {
                env::remove_var("ORCA_MANAGED_PACKAGE_ROOT");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn node_available_accepts_sibling_node_bin_from_path_layout() {
        let _guard = crate::history::lock_test_env();
        let previous_path = env::var_os("PATH");
        let previous_node_path = env::var_os("ORCA_NODE_PATH");
        let previous_node = env::var_os("ORCA_NODE");
        let previous_package_root = env::var_os("ORCA_MANAGED_PACKAGE_ROOT");
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("dependencies").join("bin");
        let node_bin = temp.path().join("dependencies").join("node").join("bin");
        std::fs::create_dir_all(&bin).expect("create fake bin");
        std::fs::create_dir_all(&node_bin).expect("create fake node bin");
        write_fake_node(&node_bin);

        unsafe {
            env::set_var("PATH", &bin);
            env::remove_var("ORCA_NODE_PATH");
            env::remove_var("ORCA_NODE");
            env::remove_var("ORCA_MANAGED_PACKAGE_ROOT");
        }

        assert!(WorkflowHost::node_available());

        unsafe {
            if let Some(previous) = previous_path {
                env::set_var("PATH", previous);
            } else {
                env::remove_var("PATH");
            }
            if let Some(previous) = previous_node_path {
                env::set_var("ORCA_NODE_PATH", previous);
            } else {
                env::remove_var("ORCA_NODE_PATH");
            }
            if let Some(previous) = previous_node {
                env::set_var("ORCA_NODE", previous);
            } else {
                env::remove_var("ORCA_NODE");
            }
            if let Some(previous) = previous_package_root {
                env::set_var("ORCA_MANAGED_PACKAGE_ROOT", previous);
            } else {
                env::remove_var("ORCA_MANAGED_PACKAGE_ROOT");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn event_callback_error_reaps_workflow_process_group() {
        use std::ffi::CString;
        use std::os::unix::fs::OpenOptionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let survivor_marker = temp.path().join("workflow-survivor");
        let gate = temp.path().join("descendant-gate");
        let gate_path = CString::new(gate.as_os_str().as_encoded_bytes()).expect("FIFO path");
        assert_eq!(unsafe { libc::mkfifo(gate_path.as_ptr(), 0o600) }, 0);
        let gate_keeper = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate)
            .expect("open descendant gate");
        let gate_writer = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&gate)
            .expect("keep descendant waiting for release");
        let node = write_fake_node_script(
            temp.path(),
            r#"#!/bin/sh
(
  exec 3< "$ORCA_WORKFLOW_TEST_GATE"
  printf '{"type":"phase_started","name":"scan"}\n'
  exec >/dev/null 2>&1
  read -r release <&3
  : > "$ORCA_WORKFLOW_TEST_MARKER"
) &
wait
"#,
        );
        let mut command = Command::new(&node);
        command
            .env("ORCA_WORKFLOW_TEST_GATE", &gate)
            .env("ORCA_WORKFLOW_TEST_MARKER", &survivor_marker);

        let script = temp.path().join("unused-workflow.js");
        fs::write(&script, "").unwrap();
        let error = WorkflowHost::run_collecting_events_with_agent_and_event_callback_inner(
            command,
            &script,
            serde_json::json!(null),
            None,
            WorkflowHostRunPolicy::collecting(),
            |_| unreachable!("fixture does not emit agent calls"),
            |_| Err(io::Error::other("event callback failed")),
            || Ok(false),
            || {},
        )
        .expect_err("event callback should fail");

        assert_eq!(error.to_string(), "event callback failed");
        drop(gate_keeper);
        // Keep a writer open so a surviving descendant cannot exit on EOF.
        // Wait for signal delivery to close its read end; reaping the parent
        // does not synchronize descendant exit.
        let deadline = Instant::now() + Duration::from_secs(2);
        let gate_error = loop {
            match fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&gate)
            {
                Err(error) => break error,
                Ok(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "workflow descendant continued after callback error"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        assert_eq!(gate_error.raw_os_error(), Some(libc::ENXIO));
        drop(gate_writer);
        assert!(
            !survivor_marker.exists(),
            "workflow descendant continued after callback error"
        );
    }

    #[test]
    #[cfg(unix)]
    fn workflow_host_drains_stderr_while_reading_events() {
        let _guard = crate::history::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let pid_path = temp.path().join("workflow-host.pid");
        let node = write_fake_node_script(
            temp.path(),
            r#"#!/bin/sh
printf '%s' "$$" > "$ORCA_WORKFLOW_TEST_PID"
i=0
while [ "$i" -lt 20000 ]; do
  printf 'workflow stderr padding 0123456789012345678901234567890123456789\n' >&2
  i=$((i + 1))
done
printf '{"type":"workflow_completed","result":{"ok":true}}\n'
"#,
        );
        let _node_path = TestEnvVar::set("ORCA_NODE_PATH", &node);
        let _pid_path = TestEnvVar::set("ORCA_WORKFLOW_TEST_PID", &pid_path);
        let script = temp.path().join("unused-workflow.js");
        fs::write(&script, "").unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = WorkflowHost::run_collecting_events(&script, serde_json::json!(null));
            let _ = sender.send(result);
        });

        let result = receiver.recv_timeout(std::time::Duration::from_secs(8));
        if result.is_err() {
            terminate_fixture_process(&pid_path);
            let _ = receiver.recv_timeout(std::time::Duration::from_secs(8));
        }
        worker.join().expect("workflow host worker");

        let events = result
            .expect("workflow host blocked on its stderr pipe")
            .expect("workflow host result");
        assert!(events.iter().any(|event| {
            matches!(event, HostEvent::WorkflowCompleted { result } if result["ok"] == true)
        }));
    }

    #[test]
    fn workflow_stderr_reader_bounds_retained_output() {
        let observed_bytes = MAX_WORKFLOW_HOST_STDERR_BYTES + 4096;
        let stderr = std::io::Cursor::new(vec![b'x'; observed_bytes]);

        let retained = join_host_thread(
            spawn_retained_reader(stderr, MAX_WORKFLOW_HOST_STDERR_BYTES, "stderr"),
            "stderr reader",
        )
        .expect("read stderr");

        assert_eq!(retained.observed_bytes, observed_bytes);
        assert_eq!(retained.bytes.len(), MAX_WORKFLOW_HOST_STDERR_BYTES);
        assert_eq!(retained.omitted_bytes, 4096);
        assert!(format_stderr(retained).ends_with("[4096 workflow stderr bytes omitted]"));
    }

    #[cfg(unix)]
    fn terminate_fixture_process(pid_path: &Path) {
        let Ok(pid) = std::fs::read_to_string(pid_path) else {
            return;
        };
        let _ = Command::new("/bin/kill")
            .args(["-KILL", pid.trim()])
            .status();
    }

    #[test]
    fn host_control_cancels_silent_workflow_promptly() {
        // The Node-discovery tests temporarily replace ORCA_NODE_PATH/PATH.
        // Keep discovery and launch in the same environment scope so this
        // test cannot run a fake Node that exits instead of staying silent.
        let _guard = crate::history::lock_test_env();
        if !WorkflowHost::node_available() {
            return;
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("silent.js");
        std::fs::write(
            &script,
            "export const meta = { name: 'silent', description: 'Silent host', phases: [] };\nwhile (true) {}\nexport default 'unreachable';",
        )
        .expect("write silent workflow");
        // The stop predicate waits 100ms so the child is genuinely spinning in
        // its busy-loop before we ask the host to stop. It records the instant
        // it first requests the stop, so the deadline below measures only the
        // stop-to-return propagation — not the Node process cold start, which is
        // unrelated to how promptly the host tears a running child down.
        let started = Instant::now();
        let stop_requested_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let predicate_stop_at = Arc::clone(&stop_requested_at);

        let result = WorkflowHost::run_collecting_events_with_agent_and_event_callback_inner(
            Command::new(WorkflowHost::node_executable()),
            &script,
            serde_json::Value::Null,
            None,
            WorkflowHostRunPolicy::callback_only(1, None, None),
            |call| {
                Ok(HostCommand::AgentResult {
                    call_id: call.call_id,
                    result: serde_json::Value::Null,
                })
            },
            |_| Ok(()),
            || {
                if started.elapsed() >= Duration::from_millis(100) {
                    predicate_stop_at
                        .lock()
                        .expect("stop instant")
                        .get_or_insert_with(Instant::now);
                    Ok(true)
                } else {
                    Ok(false)
                }
            },
            || {},
        );
        let returned_at = Instant::now();

        let error = result.expect_err("silent workflow should be cancelled");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let stop_requested_at = stop_requested_at
            .lock()
            .expect("stop instant")
            .expect("stop predicate never requested a stop");
        let stop_latency = returned_at.saturating_duration_since(stop_requested_at);
        assert!(
            stop_latency < Duration::from_secs(2),
            "silent workflow host did not stop promptly: {stop_latency:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn host_kills_child_that_stays_alive_after_terminal_event() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::history::lock_test_env();
        let previous_node_path = env::var_os("ORCA_NODE_PATH");
        let temp = tempfile::tempdir().expect("tempdir");
        let node = temp.path().join("lingering-node");
        std::fs::write(
            &node,
            "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"workflow_completed\",\"result\":null}'\nsleep 10\n",
        )
        .expect("write lingering node");
        let mut permissions = std::fs::metadata(&node)
            .expect("lingering node metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&node, permissions).expect("chmod lingering node");
        let script = temp.path().join("unused.js");
        std::fs::write(&script, " ".repeat(2 * 1024 * 1024)).expect("write workflow");

        unsafe {
            env::set_var("ORCA_NODE_PATH", &node);
        }
        let started = Instant::now();
        let result = WorkflowHost::run_collecting_events(&script, serde_json::Value::Null);

        let error = result.expect_err("lingering host should be killed");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "lingering workflow host exceeded cleanup deadline: {:?}",
            started.elapsed()
        );

        unsafe {
            if let Some(previous) = previous_node_path {
                env::set_var("ORCA_NODE_PATH", previous);
            } else {
                env::remove_var("ORCA_NODE_PATH");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn host_cleans_pipe_holding_descendants_after_parent_exit() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::history::lock_test_env();
        let previous_node_path = env::var_os("ORCA_NODE_PATH");
        let temp = tempfile::tempdir().expect("tempdir");
        let node = temp.path().join("background-node");
        std::fs::write(
            &node,
            "#!/bin/sh\nsleep 30 &\nprintf '%s\\n' '{\"type\":\"workflow_completed\",\"result\":null}'\nexit 0\n",
        )
        .expect("write background node");
        let mut permissions = std::fs::metadata(&node)
            .expect("background node metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&node, permissions).expect("chmod background node");
        let script = temp.path().join("unused.js");
        std::fs::write(&script, "export default null;").expect("write workflow");

        unsafe {
            env::set_var("ORCA_NODE_PATH", &node);
        }
        let started = Instant::now();
        let events = WorkflowHost::run_collecting_events(&script, serde_json::Value::Null)
            .expect("clean parent exit should complete");

        assert!(
            started.elapsed() < Duration::from_secs(8),
            "pipe-holding descendant exceeded cleanup deadline: {:?}",
            started.elapsed()
        );
        assert!(matches!(
            events.last(),
            Some(HostEvent::WorkflowCompleted { .. })
        ));

        unsafe {
            if let Some(previous) = previous_node_path {
                env::set_var("ORCA_NODE_PATH", previous);
            } else {
                env::remove_var("ORCA_NODE_PATH");
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn host_failure_stderr_is_bounded() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::history::lock_test_env();
        let previous_node_path = env::var_os("ORCA_NODE_PATH");
        let temp = tempfile::tempdir().expect("tempdir");
        let node = temp.path().join("noisy-node");
        std::fs::write(
            &node,
            "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"workflow_completed\",\"result\":null}'\ndd if=/dev/zero bs=2097152 count=1 2>/dev/null | tr '\\000' x >&2\nexit 7\n",
        )
        .expect("write noisy node");
        let mut permissions = std::fs::metadata(&node)
            .expect("noisy node metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&node, permissions).expect("chmod noisy node");
        let script = temp.path().join("unused.js");
        std::fs::write(&script, "export default null;").expect("write workflow");

        unsafe {
            env::set_var("ORCA_NODE_PATH", &node);
        }
        let error = WorkflowHost::run_collecting_events(&script, serde_json::Value::Null)
            .expect_err("noisy host should fail");

        assert!(error.to_string().len() < 100_000);
        assert!(error.to_string().contains("bytes omitted"));

        unsafe {
            if let Some(previous) = previous_node_path {
                env::set_var("ORCA_NODE_PATH", previous);
            } else {
                env::remove_var("ORCA_NODE_PATH");
            }
        }
    }

    #[test]
    fn host_command_json_is_bounded_before_pipe_write() {
        let writer = Arc::new(Mutex::new(Vec::new()));
        let command = HostCommand::AgentResult {
            call_id: "call-large".to_string(),
            result: serde_json::Value::String("x".repeat(MAX_WORKFLOW_HOST_FRAME_BYTES * 2)),
        };

        let error = write_host_command(&writer, &command)
            .expect_err("oversized host command should fail closed");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("command frame"));
        assert!(writer.lock().expect("writer").is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn the_workflow_host_does_not_inherit_orcas_credentials() {
        let temp = tempfile::tempdir().expect("tempdir");
        let seen = temp.path().join("environment");
        let node = write_fake_node_script(
            temp.path(),
            "#!/bin/sh\nenv > \"$ORCA_WORKFLOW_TEST_ENV\"\n",
        );
        let mut command = Command::new(&node);
        command
            .env("ORCA_WORKFLOW_TEST_ENV", &seen)
            .env("DEEPSEEK_API_KEY", "sk-workflow-test")
            .env("ORCA_API_KEY", "sk-workflow-test")
            .env("BRAVE_SEARCH_API_KEY", "sk-workflow-test");

        let script = temp.path().join("unused-workflow.js");
        fs::write(&script, "").unwrap();
        let _ = WorkflowHost::run_collecting_events_with_agent_and_event_callback_inner(
            command,
            &script,
            serde_json::json!(null),
            None,
            WorkflowHostRunPolicy::collecting(),
            |_| unreachable!("fixture does not emit agent calls"),
            |_| Ok(()),
            || Ok(false),
            || {},
        );

        let environment = fs::read_to_string(&seen).expect("the fake node ran");
        assert!(environment.contains("ORCA_WORKFLOW_TEST_ENV="));
        assert!(
            !environment.contains("sk-workflow-test"),
            "the workflow host was started with a credential:\n{environment}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_node_without_the_permission_model_is_not_given_its_flags() {
        let temp = tempfile::tempdir().expect("tempdir");
        let node = write_fake_node(temp.path());

        let args = node_confinement_args(
            &node,
            &[Path::new("/host.mjs"), Path::new("/workflow.js")],
            None,
            None,
        );

        assert_eq!(args, ["--disallow-code-generation-from-strings"]);
    }

    /// `node --version`, as (major, minor).
    #[cfg(unix)]
    fn node_version(node: &Path) -> Option<(u32, u32)> {
        let output = Command::new(node).arg("--version").output().ok()?;
        let version = String::from_utf8(output.stdout).ok()?;
        let mut parts = version.trim().strip_prefix('v')?.split('.');
        Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
    }

    // Unix only: whether a Windows temp directory's path suits Node's
    // permission model has not been checked, and there the host falls back
    // to running without it.
    #[test]
    #[cfg(unix)]
    fn a_node_that_has_the_permission_model_is_started_with_it() {
        // See `run_confined`: the node found must be the real one.
        let _env = crate::history::lock_test_env();
        let node = WorkflowHost::node_executable();
        // `--permission` is in Node 22 from 22.13, in 23 from 23.5, and in
        // every later line.
        let has_flag = match node_version(&node) {
            Some((22, minor)) => minor >= 13,
            Some((23, minor)) => minor >= 5,
            Some((major, _)) => major >= 24,
            None => false,
        };
        if !has_flag {
            return;
        }

        assert!(
            node_has_permission_model(&node),
            "the workflow host would run on this Node without the permission model"
        );
    }

    /// Runs `module` as the workflow host would be run, with `readable` as the
    /// workflow script, and returns what it printed.
    fn run_confined(module: &str, readable: &Path, ipc: Option<&WorkflowHostIpcPaths>) -> String {
        // The node lookup tests point `PATH` at a fake node in a temporary
        // directory, under this lock, and delete it when they end. Held
        // until node has run, so the node found here is the real one and
        // still exists when it is started.
        let _env = crate::history::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("entry.mjs");
        fs::write(&entry, module).expect("write entry module");
        let entry = node_entry_path(entry);
        let node = WorkflowHost::node_executable();
        let output = Command::new(&node)
            .args(node_confinement_args(&node, &[&entry, readable], ipc, None))
            .arg(&entry)
            .arg(readable)
            .output()
            .expect("run node");
        assert!(
            output.status.success(),
            "node failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf8 output")
    }

    #[test]
    fn the_workflow_host_process_reads_its_script_and_nothing_else() {
        let node = WorkflowHost::node_executable();
        if !WorkflowHost::node_available() || !node_has_permission_model(&node) {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("workflow.js");
        let private = temp.path().join("private.txt");
        fs::write(&script, "script").expect("write script");
        fs::write(&private, "private").expect("write private file");

        let seen = run_confined(
            r#"
import { readFileSync, writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
const script = process.argv[2];
const attempt = (action) => { try { action(); return "allowed"; } catch (error) { return error.code; } };
console.log(JSON.stringify({
  script: attempt(() => readFileSync(script)),
  neighbour: attempt(() => readFileSync(join(dirname(script), "private.txt"))),
  write: attempt(() => writeFileSync(join(dirname(script), "made.txt"), "x")),
  command: attempt(() => execFileSync(process.execPath, ["--version"])),
  code: attempt(() => Function("return 1")()),
}));
"#,
            &script,
            None,
        );

        let seen: Value = serde_json::from_str(seen.trim()).expect("json report");
        assert_eq!(seen["script"], "allowed");
        assert_eq!(seen["neighbour"], "ERR_ACCESS_DENIED");
        assert_eq!(seen["write"], "ERR_ACCESS_DENIED");
        assert_eq!(seen["command"], "ERR_ACCESS_DENIED");
        assert_ne!(seen["code"], "allowed");
        assert!(!temp.path().join("made.txt").exists());
    }

    #[test]
    fn workflow_task_list_call_waits_for_the_reply_file() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("workflow.js");
        fs::write(
            &script,
            "export const meta = { name: 'retry', description: 'Wait for task list reply', phases: [] };
             export default createTaskList('audit', ['check'])[0].status;",
        )
        .unwrap();
        let preload = temp.path().join("reply-reads.cjs");
        fs::write(
            &preload,
            r#"const fs = require('node:fs');
const { syncBuiltinESMExports } = require('node:module');
const readFileSync = fs.readFileSync;
const codes = ['EBUSY', 'EBUSY', 'EPERM', 'EPERM', 'EACCES', 'EACCES'];
fs.readFileSync = (path, ...options) => {
  if (path === process.argv[5] && codes.length > 0) {
    const code = codes.shift();
    throw Object.assign(new Error(`Reply read failed with ${code}`), { code });
  }
  return readFileSync(path, ...options);
};
syncBuiltinESMExports();
"#,
        )
        .unwrap();

        let mut command = Command::new(WorkflowHost::node_executable());
        command.arg("--require").arg(node_entry_path(preload));
        let events = WorkflowHost::run_collecting_events_with_agent_and_event_callback_inner(
            command,
            &script,
            Value::Null,
            None,
            WorkflowHostRunPolicy::collecting(),
            |_| unreachable!("script makes no agent calls"),
            |_| Ok(()),
            || Ok(false),
            || {},
        )
        .unwrap();
        assert!(
            matches!(events.last(), Some(HostEvent::WorkflowCompleted { result }) if result == "pending"),
            "last host event: {:?}",
            events.last()
        );
    }

    #[test]
    fn workflow_task_list_reports_oversized_requests_to_the_script() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("workflow.js");
        fs::write(
            &script,
            "export const meta = { name: 'limit', description: 'Task list limit', phases: [] };
             let error;
             try { createTaskList('oversized', ['界'.repeat(350_000)]); }
             catch (e) { error = e.message; }
             export default [error, listTasks('oversized').length, createTaskList('small', ['ok']).length];",
        )
        .unwrap();
        let events = WorkflowHost::run_collecting_events_with_agent(&script, Value::Null, |_| {
            unreachable!("script makes no agent calls")
        })
        .unwrap();
        assert!(
            matches!(events.last(), Some(HostEvent::WorkflowCompleted { result })
            if *result == serde_json::json!(["Workflow task list request exceeds 1048576 bytes", 0, 1]))
        );
    }

    #[test]
    fn workflow_task_list_call_completes_with_an_agent_backlog() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("workflow.js");
        fs::write(
            &script,
            "export const meta = { name: 'backlog', description: 'Task list with agents', phases: [] };
             const agents = Array.from({ length: 64 }, () => agent('work'));
             const tasks = createTaskList('audit', ['check']);
             await Promise.all(agents);
             export default tasks[0].status;",
        )
        .unwrap();

        let output = WorkflowHost::run_collecting_events_with_agent(&script, Value::Null, |call| {
            Ok(HostCommand::AgentResult {
                call_id: call.call_id,
                result: Value::String("x".repeat(256 * 1024)),
            })
        })
        .unwrap();
        assert!(
            matches!(
                output.last(),
                Some(HostEvent::WorkflowCompleted { result }) if result == "pending"
            ),
            "last host event: {:?}",
            output.last()
        );
    }

    #[test]
    fn the_workflow_host_process_writes_only_its_mailbox() {
        let node = WorkflowHost::node_executable();
        if !WorkflowHost::node_available() || !node_has_permission_model(&node) {
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("workflow.js");
        fs::write(&script, "script").expect("write script");
        fs::write(temp.path().join("state.json"), "{}").expect("write run state");
        let ipc = WorkflowHostIpcPaths {
            mailbox_path: temp.path().join("mailbox.json"),
            task_lists_path: temp.path().join("task-lists.json"),
        };

        let seen = run_confined(
            r#"
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
const run = dirname(process.argv[2]);
const attempt = (action) => { try { action(); return "allowed"; } catch (error) { return error.code; } };
console.log(JSON.stringify({
  mailbox: attempt(() => writeFileSync(join(run, "mailbox.json"), "{}")),
  tasks: attempt(() => writeFileSync(join(run, "task-lists.json"), "{}")),
  readBack: attempt(() => readFileSync(join(run, "mailbox.json"))),
  state: attempt(() => writeFileSync(join(run, "state.json"), "tampered")),
}));
"#,
            &script,
            Some(&ipc),
        );

        let seen: Value = serde_json::from_str(seen.trim()).expect("json report");
        assert_eq!(seen["mailbox"], "allowed");
        assert_eq!(seen["tasks"], "ERR_ACCESS_DENIED");
        assert_eq!(seen["readBack"], "allowed");
        assert_eq!(seen["state"], "ERR_ACCESS_DENIED");
        assert_eq!(
            fs::read_to_string(temp.path().join("state.json")).expect("read run state"),
            "{}"
        );
    }

    #[cfg(unix)]
    fn write_fake_node(dir: &Path) -> PathBuf {
        write_fake_node_script(dir, "#!/bin/sh\nexit 0\n")
    }

    #[cfg(unix)]
    fn write_fake_node_script(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let node = dir.join("node");
        std::fs::write(&node, script).expect("write fake node");
        let mut permissions = std::fs::metadata(&node)
            .expect("fake node metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&node, permissions).expect("chmod fake node");
        node
    }
}
