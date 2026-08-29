use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use orca_core::capability::CapabilitySet;
use orca_core::execution_broker::{ExecutionBroker, LaunchError};
use orca_core::retained_output::{RetainedOutput, RetainedOutputSnapshot};
use orca_platform::process::ProcessJob;

const MAX_WORKFLOW_HOST_FRAME_BYTES: usize = 1024 * 1024;
const MAX_WORKFLOW_HOST_EVENTS: usize = 8_192;
const MAX_WORKFLOW_HOST_EVENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_WORKFLOW_HOST_STDERR_BYTES: usize = 64 * 1024;
const WORKFLOW_HOST_FRAME_CHANNEL_CAPACITY: usize = 8;
const DEFAULT_WORKFLOW_HOST_AGENT_WORKERS: usize = 16;
const WORKFLOW_HOST_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const WORKFLOW_HOST_EXIT_GRACE: Duration = Duration::from_secs(2);

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
struct WorkflowHostRunPolicy {
    collect_events: bool,
    max_agent_workers: usize,
}

impl WorkflowHostRunPolicy {
    fn collecting() -> Self {
        Self {
            collect_events: true,
            max_agent_workers: DEFAULT_WORKFLOW_HOST_AGENT_WORKERS,
        }
    }

    fn callback_only(max_agent_workers: usize) -> Self {
        Self {
            collect_events: false,
            max_agent_workers: max_agent_workers.max(1),
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
        script_path: &Path,
        args: Value,
        ipc_paths: &WorkflowHostIpcPaths,
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
            script_path,
            args,
            Some(ipc_paths),
            WorkflowHostRunPolicy::callback_only(max_agent_workers),
            on_agent_call,
            on_event,
            should_cancel,
            on_abort,
        )
        .map(|_| ())
    }

    fn run_collecting_events_with_agent_and_event_callback_inner<F, E, C, A>(
        script_path: &Path,
        args: Value,
        ipc_paths: Option<&WorkflowHostIpcPaths>,
        policy: WorkflowHostRunPolicy,
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
        let host_path = ensure_host_file()?;
        let _host_file = WorkflowHostFileGuard::new(host_path.clone());
        let args_json = serialize_bounded_json(
            &args,
            MAX_WORKFLOW_HOST_FRAME_BYTES,
            "workflow host arguments",
        )?;
        let args_json = String::from_utf8(args_json)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

        let mut command = Command::new(Self::node_executable());
        command.arg(&host_path).arg(script_path).arg(args_json);
        if let Some(ipc_paths) = ipc_paths {
            command
                .arg(&ipc_paths.mailbox_path)
                .arg(&ipc_paths.task_lists_path);
        }
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
        let on_agent_call = &on_agent_call;
        let execution_result = thread::scope(|scope| -> io::Result<()> {
            let worker_count = policy.max_agent_workers.max(1);
            let call_queue_capacity = worker_count.saturating_mul(2).max(1);
            let (call_tx, call_rx) = crossbeam_channel::bounded::<AgentCall>(call_queue_capacity);
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

                    let event: HostEvent = serde_json::from_slice(&frame)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
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
                        send_workflow_agent_call(
                            &call_tx,
                            AgentCall {
                                call_id: call_id.clone(),
                                call_path: call_path.clone(),
                                phase: phase.clone(),
                                prompt: prompt.clone(),
                                opts: opts.clone(),
                            },
                            &should_cancel,
                        )?;
                    }
                    if policy.collect_events {
                        events.push(event);
                    }
                    if is_terminal {
                        on_abort();
                        abort_workers.store(true, Ordering::Release);
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

enum WorkflowHostFrame {
    Data(Vec<u8>),
    Eof,
    Error(io::Error),
}

struct WorkflowHostExit {
    status: ExitStatus,
    forced: bool,
}

struct WorkflowHostFileGuard {
    path: PathBuf,
}

impl WorkflowHostFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for WorkflowHostFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
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

fn send_workflow_agent_call<C>(
    sender: &crossbeam_channel::Sender<AgentCall>,
    mut call: AgentCall,
    should_cancel: &C,
) -> io::Result<()>
where
    C: Fn() -> io::Result<bool>,
{
    loop {
        match sender.try_send(call) {
            Ok(()) => return Ok(()),
            Err(crossbeam_channel::TrySendError::Full(returned)) => {
                call = returned;
                if should_cancel()? {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "workflow host cancelled",
                    ));
                }
                thread::sleep(WORKFLOW_HOST_CONTROL_POLL_INTERVAL);
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                return Err(io::Error::other(
                    "workflow host agent worker queue disconnected",
                ));
            }
        }
    }
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
            continue;
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

fn ensure_host_file() -> io::Result<PathBuf> {
    static HOST_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

    let seq = HOST_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "orca-workflow-host-{}-{seq}.mjs",
        std::process::id()
    ));
    fs::write(&path, include_str!("host.mjs"))?;
    Ok(path)
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

    #[test]
    fn host_file_paths_are_unique_for_parallel_tests() {
        let first = ensure_host_file().unwrap();
        let second = ensure_host_file().unwrap();

        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
        std::fs::remove_file(first).expect("remove first host file");
        std::fs::remove_file(second).expect("remove second host file");
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
        let _guard = crate::history::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let survivor_marker = temp.path().join("workflow-survivor");
        let node = write_fake_node_script(
            temp.path(),
            r#"#!/bin/sh
(sleep 0.4; : > "$ORCA_WORKFLOW_TEST_MARKER") &
printf '{"type":"phase_started","name":"scan"}\n'
wait
"#,
        );
        let _node_path = TestEnvVar::set("ORCA_NODE_PATH", &node);
        let _marker = TestEnvVar::set("ORCA_WORKFLOW_TEST_MARKER", &survivor_marker);

        let error = WorkflowHost::run_collecting_events_with_agent_and_event_callback(
            &temp.path().join("unused-workflow.js"),
            serde_json::json!(null),
            |_| unreachable!("fixture does not emit agent calls"),
            |_| Err(io::Error::other("event callback failed")),
        )
        .expect_err("event callback should fail");

        assert_eq!(error.to_string(), "event callback failed");
        thread::sleep(std::time::Duration::from_millis(600));
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
            &script,
            serde_json::Value::Null,
            None,
            WorkflowHostRunPolicy::callback_only(1),
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
        std::fs::write(&script, "export default null;").expect("write workflow");

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
