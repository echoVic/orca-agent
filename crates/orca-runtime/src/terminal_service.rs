use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use orca_core::config::{PermissionProfileNetworkAccess, RunConfig};
use orca_core::task_types::TaskStatus;
use serde::Serialize;

use crate::lifecycle::TurnPermissionOverlay;
use crate::network_proxy::{RuntimeNetworkPolicy, RuntimeNetworkProxy};
use crate::shell_session::{
    RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellSessionHandle,
    ShellSessionOutput, ShellSessionTermination, ShellTerminalMode,
};
use crate::tasks::TaskRegistry;

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const TERMINAL_COMMAND_CAPACITY: usize = 32;
const COMPLETION_QUEUE_CAPACITY: usize = 64;
const COMPLETION_OUTPUT_MAX_BYTES: usize = 8 * 1024;
const COMPLETED_SESSION_RETENTION: Duration = Duration::from_secs(10 * 60);

pub(crate) struct TerminalService {
    sender: SyncSender<TerminalCommand>,
    supervisor: Mutex<Option<thread::JoinHandle<()>>>,
}

struct TerminalServiceState {
    manager: RuntimeShellSessionManager,
    sessions: HashMap<String, TerminalSessionState>,
    completions: VecDeque<TerminalCompletion>,
}

struct TerminalSessionState {
    task_id: String,
    cursor: usize,
    requested_terminal: ShellTerminalMode,
    effective_terminal: ShellTerminalMode,
    terminal: Option<TerminalState>,
    background_notifiable: bool,
    completion_observed: bool,
    completion_queued: bool,
    completed_at: Option<Instant>,
    network_proxy: Option<RuntimeNetworkProxy>,
}

#[derive(Clone, Copy)]
struct TerminalState {
    status: TaskStatus,
    termination: ShellSessionTermination,
    exit_code: Option<i32>,
}

enum TerminalCommand {
    Start {
        command: Box<ShellSessionCommand>,
        metadata_writable_directories: Vec<PathBuf>,
        network_proxy: Option<RuntimeNetworkProxy>,
        response: SyncSender<io::Result<ShellSessionHandle>>,
    },
    Write {
        session_id: String,
        chars: String,
        response: SyncSender<io::Result<()>>,
    },
    Poll {
        session_id: String,
        max_output_bytes: usize,
        response: SyncSender<io::Result<TerminalServiceOutput>>,
    },
    MarkBackground {
        session_id: String,
    },
    StopTask {
        task_id: String,
        response: SyncSender<io::Result<bool>>,
    },
    DrainCompletions {
        response: SyncSender<Vec<TerminalCompletion>>,
    },
    Shutdown {
        response: SyncSender<()>,
    },
}

pub(crate) struct TerminalExecRequest<'a> {
    pub(crate) command: &'a str,
    pub(crate) cwd: &'a Path,
    pub(crate) additional_roots: &'a [PathBuf],
    pub(crate) config: Option<&'a RunConfig>,
    pub(crate) permission_overlay: &'a TurnPermissionOverlay,
    pub(crate) terminal: ShellTerminalMode,
    #[cfg(test)]
    pub(crate) sandbox_override: Option<ShellSandboxMode>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalServiceOutput {
    pub(crate) session_id: String,
    pub(crate) task_id: String,
    pub(crate) status: &'static str,
    pub(crate) termination: &'static str,
    pub(crate) output: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) truncated: bool,
    pub(crate) omitted_prefix_bytes: usize,
    pub(crate) next_output_offset: usize,
    pub(crate) output_bytes_total: usize,
    pub(crate) requested_terminal: &'static str,
    pub(crate) effective_terminal: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalCompletion {
    pub(crate) session_id: String,
    pub(crate) task_id: String,
    pub(crate) status: &'static str,
    pub(crate) output: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) truncated: bool,
}

impl TerminalCompletion {
    pub(crate) fn model_notification(&self) -> String {
        let exit_code = self
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let output = if self.output.trim().is_empty() {
            "(no output)"
        } else {
            self.output.trim_end()
        };
        let truncated = if self.truncated {
            "\n[completion output truncated]"
        } else {
            ""
        };
        format!(
            "<task-notification>Terminal session {} (task {}) finished with status {} and exit code {}.\n{}{}{}</task-notification>",
            self.session_id,
            self.task_id,
            self.status,
            exit_code,
            output,
            truncated,
            if self.status == "completed" {
                ""
            } else {
                "\nInspect the output before continuing."
            },
        )
    }
}

impl TerminalService {
    pub(crate) fn new(task_registry: TaskRegistry) -> Self {
        let (sender, receiver) = mpsc::sync_channel(TERMINAL_COMMAND_CAPACITY);
        let supervisor = thread::Builder::new()
            .name("orca-terminal-supervisor".to_string())
            .spawn(move || run_terminal_supervisor(task_registry, receiver))
            .expect("terminal supervisor thread must start");
        Self {
            sender,
            supervisor: Mutex::new(Some(supervisor)),
        }
    }

    pub(crate) fn exec(
        &self,
        request: TerminalExecRequest<'_>,
        yield_time: Duration,
        max_output_bytes: usize,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<TerminalServiceOutput> {
        let (command, metadata_writable_directories, network_proxy) =
            prepare_shell_command(request)?;
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(TerminalCommand::Start {
            command: Box::new(command),
            metadata_writable_directories,
            network_proxy,
            response,
        })?;
        let handle = receive_response(receiver, "terminal start")??;
        let output = self.poll_until(
            &handle.id,
            yield_time,
            max_output_bytes,
            false,
            should_cancel,
        )?;
        if output.status == "running" {
            let _ = self.send(TerminalCommand::MarkBackground {
                session_id: handle.id,
            });
        }
        Ok(output)
    }

    pub(crate) fn write_stdin(
        &self,
        session_id: &str,
        chars: Option<&str>,
        yield_time: Duration,
        max_output_bytes: usize,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<TerminalServiceOutput> {
        if let Some(chars) = chars.filter(|chars| !chars.is_empty()) {
            let (response, receiver) = mpsc::sync_channel(1);
            self.send(TerminalCommand::Write {
                session_id: session_id.to_string(),
                chars: chars.to_string(),
                response,
            })?;
            receive_response(receiver, "terminal write")??;
        }
        self.poll_until(
            session_id,
            yield_time,
            max_output_bytes,
            true,
            should_cancel,
        )
    }

    pub(crate) fn stop_task(&self, task_id: &str) -> io::Result<bool> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(TerminalCommand::StopTask {
            task_id: task_id.to_string(),
            response,
        })?;
        receive_response(receiver, "terminal stop")?
    }

    pub(crate) fn drain_completions(&self) -> Vec<TerminalCompletion> {
        let (response, receiver) = mpsc::sync_channel(1);
        if self
            .send(TerminalCommand::DrainCompletions { response })
            .is_err()
        {
            return Vec::new();
        }
        receiver.recv().unwrap_or_default()
    }

    fn poll_until(
        &self,
        session_id: &str,
        yield_time: Duration,
        max_output_bytes: usize,
        return_on_output: bool,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<TerminalServiceOutput> {
        let deadline = Instant::now()
            .checked_add(yield_time)
            .unwrap_or_else(Instant::now);
        let mut aggregate: Option<TerminalServiceOutput> = None;
        let mut remaining_output_bytes = max_output_bytes.max(1);
        loop {
            let output = self.poll_once(session_id, remaining_output_bytes)?;
            let observed_output = output.output.len();
            let status = output.status;
            merge_terminal_output(&mut aggregate, output);
            remaining_output_bytes = remaining_output_bytes.saturating_sub(observed_output);
            if status != "running"
                || (return_on_output
                    && aggregate
                        .as_ref()
                        .is_some_and(|output| !output.output.is_empty()))
                || remaining_output_bytes == 0
                || Instant::now() >= deadline
                || should_cancel()
            {
                return Ok(aggregate.expect("terminal poll always produces output metadata"));
            }
            std::thread::sleep(
                POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    fn poll_once(
        &self,
        session_id: &str,
        max_output_bytes: usize,
    ) -> io::Result<TerminalServiceOutput> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(TerminalCommand::Poll {
            session_id: session_id.to_string(),
            max_output_bytes,
            response,
        })?;
        receive_response(receiver, "terminal poll")?
    }

    fn send(&self, command: TerminalCommand) -> io::Result<()> {
        self.sender
            .send(command)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "terminal supervisor stopped"))
    }
}

impl Drop for TerminalService {
    fn drop(&mut self) {
        let (response, receiver) = mpsc::sync_channel(1);
        let _ = self.sender.send(TerminalCommand::Shutdown { response });
        let _ = receiver.recv_timeout(Duration::from_secs(2));
        if let Some(supervisor) = self
            .supervisor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = supervisor.join();
        }
    }
}

fn receive_response<T>(receiver: Receiver<T>, operation: &str) -> io::Result<T> {
    receiver.recv().map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("{operation} response channel closed"),
        )
    })
}

fn run_terminal_supervisor(task_registry: TaskRegistry, receiver: Receiver<TerminalCommand>) {
    let mut state = TerminalServiceState {
        manager: RuntimeShellSessionManager::new(task_registry),
        sessions: HashMap::new(),
        completions: VecDeque::new(),
    };
    loop {
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(TerminalCommand::Start {
                command,
                metadata_writable_directories,
                network_proxy,
                response,
            }) => {
                let result = state
                    .manager
                    .spawn_with_metadata_roots(*command, metadata_writable_directories);
                if let Ok(handle) = &result {
                    state.sessions.insert(
                        handle.id.clone(),
                        TerminalSessionState::from_handle(handle, network_proxy),
                    );
                }
                let _ = response.send(result);
            }
            Ok(TerminalCommand::Write {
                session_id,
                chars,
                response,
            }) => {
                let result = state.write(&session_id, &chars);
                let _ = response.send(result);
            }
            Ok(TerminalCommand::Poll {
                session_id,
                max_output_bytes,
                response,
            }) => {
                let result = state.poll(&session_id, max_output_bytes);
                let _ = response.send(result);
            }
            Ok(TerminalCommand::MarkBackground { session_id }) => {
                state.mark_background(&session_id);
            }
            Ok(TerminalCommand::StopTask { task_id, response }) => {
                let result = state.stop_task(&task_id);
                let _ = response.send(result);
            }
            Ok(TerminalCommand::DrainCompletions { response }) => {
                let _ = response.send(state.completions.drain(..).collect());
            }
            Ok(TerminalCommand::Shutdown { response }) => {
                state.manager.terminate_all();
                let _ = response.send(());
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                state.manager.terminate_all();
                break;
            }
        }
        let _ = state.reap();
        state.cleanup_completed();
    }
}

impl TerminalServiceState {
    fn write(&mut self, session_id: &str, chars: &str) -> io::Result<()> {
        let session = self.sessions.get(session_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown terminal session: {session_id}"),
            )
        })?;
        if session.terminal.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("terminal session has already completed: {session_id}"),
            ));
        }
        self.manager.write_stdin(session_id, chars)
    }

    fn poll(
        &mut self,
        session_id: &str,
        max_output_bytes: usize,
    ) -> io::Result<TerminalServiceOutput> {
        self.reap()?;
        let (task_id, cursor, requested_terminal, effective_terminal, terminal) = {
            let session = self.sessions.get(session_id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("unknown terminal session: {session_id}"),
                )
            })?;
            (
                session.task_id.clone(),
                session.cursor,
                session.requested_terminal,
                session.effective_terminal,
                session.terminal,
            )
        };
        let output = self
            .manager
            .read_output_delta(&task_id, cursor, max_output_bytes.max(1))?;
        let terminal = terminal.unwrap_or_else(TerminalState::running);
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.cursor = output.next_offset;
        }
        if terminal.status != TaskStatus::Running {
            self.observe_completion(session_id);
        }
        if terminal.status != TaskStatus::Running && output.next_offset >= output.bytes_total {
            self.sessions.remove(session_id);
            self.manager.remove_output(&task_id);
        }
        Ok(TerminalServiceOutput {
            session_id: session_id.to_string(),
            task_id,
            status: task_status_label(terminal.status),
            termination: termination_label(terminal.termination),
            output: output.combined,
            exit_code: terminal.exit_code,
            truncated: output.omitted_prefix_bytes > 0 || output.next_offset < output.bytes_total,
            omitted_prefix_bytes: output.omitted_prefix_bytes,
            next_output_offset: output.next_offset,
            output_bytes_total: output.bytes_total,
            requested_terminal: requested_terminal.as_str(),
            effective_terminal: effective_terminal.as_str(),
        })
    }

    fn stop_task(&mut self, task_id: &str) -> io::Result<bool> {
        let session_id = self.sessions.iter().find_map(|(session_id, session)| {
            (session.task_id == task_id && session.terminal.is_none()).then(|| session_id.clone())
        });
        let Some(session_id) = session_id else {
            return Ok(false);
        };
        let output = self.manager.kill_preserving_output(&session_id)?;
        self.record_terminal(&output);
        Ok(true)
    }

    fn reap(&mut self) -> io::Result<()> {
        let mut outputs = self.manager.reap_requested_stops_preserving_output()?;
        outputs.extend(self.manager.reap_completed_preserving_output()?);
        for output in outputs {
            self.record_terminal(&output);
        }
        Ok(())
    }

    fn mark_background(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.background_notifiable = true;
        }
        self.queue_completion(session_id);
    }

    fn record_terminal(&mut self, output: &ShellSessionOutput) {
        if let Some(session) = self.sessions.get_mut(&output.id)
            && session.terminal.is_none()
        {
            session.terminal = Some(TerminalState::from_output(output));
            session.completed_at = Some(Instant::now());
            session.network_proxy.take();
        }
        self.queue_completion(&output.id);
    }

    fn queue_completion(&mut self, session_id: &str) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        let Some(terminal) = session.terminal else {
            return;
        };
        if !session.background_notifiable
            || session.completion_observed
            || session.completion_queued
        {
            return;
        }
        let task_id = session.task_id.clone();
        let output = self
            .manager
            .read_output_delta(&task_id, 0, COMPLETION_OUTPUT_MAX_BYTES)
            .ok();
        let completion = TerminalCompletion {
            session_id: session_id.to_string(),
            task_id,
            status: task_status_label(terminal.status),
            output: output
                .as_ref()
                .map(|output| output.combined.clone())
                .unwrap_or_default(),
            exit_code: terminal.exit_code,
            truncated: output.as_ref().is_some_and(|output| {
                output.omitted_prefix_bytes > 0 || output.next_offset < output.bytes_total
            }),
        };
        if self.completions.len() >= COMPLETION_QUEUE_CAPACITY {
            self.completions.pop_front();
        }
        self.completions.push_back(completion);
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.completion_queued = true;
        }
    }

    fn observe_completion(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.completion_observed = true;
        }
        self.completions
            .retain(|completion| completion.session_id != session_id);
    }

    fn cleanup_completed(&mut self) {
        let expired = self
            .sessions
            .iter()
            .filter_map(|(session_id, session)| {
                session
                    .completed_at
                    .filter(|completed_at| completed_at.elapsed() >= COMPLETED_SESSION_RETENTION)
                    .map(|_| (session_id.clone(), session.task_id.clone()))
            })
            .collect::<Vec<_>>();
        for (session_id, task_id) in expired {
            self.sessions.remove(&session_id);
            self.manager.remove_output(&task_id);
        }
    }
}

fn merge_terminal_output(
    aggregate: &mut Option<TerminalServiceOutput>,
    next: TerminalServiceOutput,
) {
    let Some(current) = aggregate.as_mut() else {
        *aggregate = Some(next);
        return;
    };
    current.status = next.status;
    current.termination = next.termination;
    current.exit_code = next.exit_code;
    current.output.push_str(&next.output);
    current.truncated |= next.truncated;
    current.omitted_prefix_bytes = current
        .omitted_prefix_bytes
        .saturating_add(next.omitted_prefix_bytes);
    current.next_output_offset = next.next_output_offset;
    current.output_bytes_total = next.output_bytes_total;
    current.requested_terminal = next.requested_terminal;
    current.effective_terminal = next.effective_terminal;
}

impl TerminalSessionState {
    fn from_handle(
        handle: &ShellSessionHandle,
        network_proxy: Option<RuntimeNetworkProxy>,
    ) -> Self {
        Self {
            task_id: handle.task_id.clone(),
            cursor: 0,
            requested_terminal: handle.requested_terminal,
            effective_terminal: handle.effective_terminal,
            terminal: None,
            background_notifiable: false,
            completion_observed: false,
            completion_queued: false,
            completed_at: None,
            network_proxy,
        }
    }
}

impl TerminalState {
    fn running() -> Self {
        Self {
            status: TaskStatus::Running,
            termination: ShellSessionTermination::Running,
            exit_code: None,
        }
    }

    fn from_output(output: &ShellSessionOutput) -> Self {
        Self {
            status: output.status,
            termination: output.termination,
            exit_code: output.exit_code,
        }
    }
}

fn prepare_shell_command(
    request: TerminalExecRequest<'_>,
) -> io::Result<(
    ShellSessionCommand,
    Vec<PathBuf>,
    Option<RuntimeNetworkProxy>,
)> {
    let mut sandbox = match request.config {
        Some(config) => {
            crate::server::bash_sandbox_for_cwd(config, request.cwd).map_err(io::Error::other)?
        }
        None => crate::server::CommandExecSandbox {
            mode: ShellSandboxMode::default(),
            additional_readable_roots: Vec::new(),
            additional_writable_roots: Vec::new(),
            metadata_writable_roots: Vec::new(),
            denied_writable_roots: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            network_policy_domains: HashMap::new(),
        },
    };
    #[cfg(test)]
    if let Some(sandbox_override) = request.sandbox_override {
        sandbox.mode = sandbox_override;
    }
    for (domain, access) in request.permission_overlay.network_domain_permissions() {
        match access {
            PermissionProfileNetworkAccess::Deny => {
                sandbox
                    .network_policy_domains
                    .insert(domain.clone(), *access);
            }
            PermissionProfileNetworkAccess::Allow => {
                sandbox
                    .network_policy_domains
                    .entry(domain.clone())
                    .or_insert(*access);
            }
        }
    }
    for root in request.permission_overlay.additional_working_directories() {
        push_unique_path(&mut sandbox.additional_writable_roots, root.clone());
    }
    for root in request.permission_overlay.metadata_writable_directories() {
        push_unique_path(&mut sandbox.metadata_writable_roots, root.clone());
    }

    #[cfg(windows)]
    if !sandbox.network_policy_domains.is_empty() {
        return Err(io::Error::other(
            "Windows domain-restricted network sandbox is unavailable; refusing to run without an OS-enforced network boundary",
        ));
    }

    let network_proxy = if sandbox.network_policy_domains.is_empty() {
        None
    } else {
        Some(RuntimeNetworkProxy::start(RuntimeNetworkPolicy::new(
            sandbox.network_policy_domains.clone(),
        ))?)
    };
    let mut env = BTreeMap::new();
    if let Some(proxy) = network_proxy.as_ref() {
        let proxy_url = proxy.proxy_url().to_string();
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            env.insert(key.to_string(), Some(proxy_url.clone()));
        }
        for key in ["NO_PROXY", "no_proxy"] {
            env.insert(key.to_string(), None);
        }
    }

    let mut additional_working_directories = request.additional_roots.to_vec();
    for root in sandbox.additional_writable_roots {
        push_unique_path(&mut additional_working_directories, root);
    }
    let metadata_writable_directories = sandbox.metadata_writable_roots;
    Ok((
        ShellSessionCommand {
            command: request.command.to_string(),
            argv: None,
            cwd: request.cwd.to_path_buf(),
            additional_readable_directories: sandbox.additional_readable_roots,
            additional_working_directories,
            denied_working_directories: sandbox.denied_writable_roots,
            allowed_unix_socket_roots: sandbox.allowed_unix_socket_roots,
            env,
            description: request.command.to_string(),
            terminal: request.terminal,
            sandbox: sandbox.mode,
        },
        metadata_writable_directories,
        network_proxy,
    ))
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Completed => "completed",
        TaskStatus::Stopped => "stopped",
        TaskStatus::Failed => "failed",
        TaskStatus::ApprovalRequired => "approval_required",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Running => "running",
        TaskStatus::Queued => "queued",
        TaskStatus::Paused => "paused",
        TaskStatus::Stopping => "stopping",
    }
}

fn termination_label(termination: ShellSessionTermination) -> &'static str {
    match termination {
        ShellSessionTermination::Running => "running",
        ShellSessionTermination::Exited => "exited",
        ShellSessionTermination::Cancelled => "cancelled",
        ShellSessionTermination::TimedOut => "timed_out",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(cwd: &Path) -> (TerminalService, TaskRegistry) {
        let registry =
            TaskRegistry::new_for_cwd(format!("terminal-test-{}", uuid::Uuid::new_v4()), cwd);
        (TerminalService::new(registry.clone()), registry)
    }

    fn request<'a>(
        command: &'a str,
        cwd: &'a Path,
        overlay: &'a TurnPermissionOverlay,
        terminal: ShellTerminalMode,
    ) -> TerminalExecRequest<'a> {
        TerminalExecRequest {
            command,
            cwd,
            additional_roots: &[],
            config: None,
            permission_overlay: overlay,
            terminal,
            sandbox_override: Some(ShellSandboxMode::DangerFullAccess),
        }
    }

    #[test]
    fn exec_returns_completed_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let overlay = TurnPermissionOverlay::default();
        let (service, _) = service(temp.path());
        let output = service
            .exec(
                request(
                    "printf unified",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_secs(2),
                8 * 1024,
                || false,
            )
            .expect("exec");

        assert_eq!(output.status, "completed", "{output:?}");
        assert_eq!(output.output, "unified");
    }

    #[test]
    fn running_session_accepts_stdin() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, _) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let started = service
            .exec(
                request(
                    "read line; printf 'got:%s' \"$line\"",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_millis(50),
                8 * 1024,
                || false,
            )
            .expect("start");
        assert_eq!(started.status, "running", "{started:?}");

        let completed = service
            .write_stdin(
                &started.session_id,
                Some("hello\n"),
                Duration::from_secs(2),
                8 * 1024,
                || false,
            )
            .expect("write stdin");
        assert_eq!(completed.status, "completed");
        assert!(completed.output.contains("got:hello"));
    }

    #[cfg(unix)]
    #[test]
    fn pty_session_applies_ctrl_u_line_kill() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, _) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let started = service
            .exec(
                request(
                    "read line; printf '\\nvalue:%s\\n' \"$line\"",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pty(Some(100), Some(30)),
                ),
                Duration::from_millis(50),
                8 * 1024,
                || false,
            )
            .expect("start pty");
        assert_eq!(started.status, "running", "{started:?}");
        assert_eq!(started.effective_terminal, "pty");

        let completed = service
            .write_stdin(
                &started.session_id,
                Some("wrong\u{15}right\n"),
                Duration::from_secs(2),
                8 * 1024,
                || false,
            )
            .expect("write terminal input");
        assert_eq!(completed.status, "completed", "{completed:?}");
        assert!(completed.output.contains("value:right"), "{completed:?}");
        assert!(!completed.output.contains("value:wrong"), "{completed:?}");
    }

    #[test]
    fn stop_task_terminates_the_owned_process() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, _) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let started = service
            .exec(
                request(
                    "printf ready; sleep 30",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_millis(50),
                8 * 1024,
                || false,
            )
            .expect("start long command");
        assert_eq!(started.status, "running", "{started:?}");
        assert!(service.stop_task(&started.task_id).expect("stop task"));

        let stopped = service
            .write_stdin(&started.session_id, None, Duration::ZERO, 8 * 1024, || {
                false
            })
            .expect("poll stopped task");
        assert_ne!(stopped.status, "running", "{stopped:?}");
        assert_eq!(stopped.termination, "cancelled", "{stopped:?}");
    }

    #[test]
    fn background_command_settles_without_poll() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, registry) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let started = service
            .exec(
                request(
                    "sleep 0.1; printf done",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_millis(10),
                8 * 1024,
                || false,
            )
            .expect("start background command");
        assert_eq!(started.status, "running", "{started:?}");

        wait_for_status(&registry, &started.task_id, TaskStatus::Completed);
        let completions = service.drain_completions();
        assert_eq!(completions.len(), 1, "{completions:?}");
        assert_eq!(completions[0].task_id, started.task_id);
        assert_eq!(completions[0].status, "completed");
        assert_eq!(completions[0].exit_code, Some(0));
        assert!(completions[0].output.contains("done"), "{completions:?}");
    }

    #[test]
    fn task_registry_stop_is_reaped_without_terminal_poll() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, registry) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let started = service
            .exec(
                request(
                    "printf ready; sleep 30",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_millis(50),
                8 * 1024,
                || false,
            )
            .expect("start long command");
        assert_eq!(started.status, "running", "{started:?}");

        registry
            .request_stop(&started.task_id)
            .expect("request task stop");
        wait_for_status(&registry, &started.task_id, TaskStatus::Stopped);
        let completions = service.drain_completions();
        assert_eq!(completions.len(), 1, "{completions:?}");
        assert_eq!(completions[0].status, "stopped");
    }

    #[test]
    fn completion_is_queued_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, registry) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let started = service
            .exec(
                request(
                    "sleep 0.1; printf once",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_millis(10),
                8 * 1024,
                || false,
            )
            .expect("start background command");
        assert_eq!(started.status, "running", "{started:?}");

        wait_for_status(&registry, &started.task_id, TaskStatus::Completed);
        thread::sleep(POLL_INTERVAL * 3);
        assert_eq!(service.drain_completions().len(), 1);
        assert!(service.drain_completions().is_empty());
    }

    #[test]
    fn polling_terminal_suppresses_completion_notification() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, registry) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let started = service
            .exec(
                request(
                    "sleep 0.1; printf observed",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_millis(10),
                8 * 1024,
                || false,
            )
            .expect("start background command");
        assert_eq!(started.status, "running", "{started:?}");

        wait_for_status(&registry, &started.task_id, TaskStatus::Completed);
        let completed = service
            .write_stdin(&started.session_id, None, Duration::ZERO, 8 * 1024, || {
                false
            })
            .expect("observe terminal output");
        assert_eq!(completed.status, "completed", "{completed:?}");
        assert!(completed.output.contains("observed"), "{completed:?}");
        assert!(service.drain_completions().is_empty());
    }

    #[test]
    fn multiple_background_sessions_keep_outputs_separate() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, registry) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let first = service
            .exec(
                request(
                    "sleep 0.1; printf first",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_millis(10),
                8 * 1024,
                || false,
            )
            .expect("start first command");
        let second = service
            .exec(
                request(
                    "sleep 0.15; printf second",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_millis(10),
                8 * 1024,
                || false,
            )
            .expect("start second command");
        assert_eq!(first.status, "running", "{first:?}");
        assert_eq!(second.status, "running", "{second:?}");

        wait_for_status(&registry, &first.task_id, TaskStatus::Completed);
        wait_for_status(&registry, &second.task_id, TaskStatus::Completed);
        let completions = service.drain_completions();
        assert_eq!(completions.len(), 2, "{completions:?}");
        let outputs = completions
            .into_iter()
            .map(|completion| (completion.task_id, completion.output))
            .collect::<HashMap<_, _>>();
        assert!(outputs[&first.task_id].contains("first"));
        assert!(!outputs[&first.task_id].contains("second"));
        assert!(outputs[&second.task_id].contains("second"));
        assert!(!outputs[&second.task_id].contains("first"));
    }

    #[test]
    fn drop_joins_supervisor_and_kills_process_tree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("leaked");
        let overlay = TurnPermissionOverlay::default();
        let (service, _) = service(temp.path());
        let started = service
            .exec(
                request(
                    "(sleep 0.5; printf leaked > leaked) & wait",
                    temp.path(),
                    &overlay,
                    ShellTerminalMode::pipe(),
                ),
                Duration::from_millis(50),
                8 * 1024,
                || false,
            )
            .expect("start process tree");
        assert_eq!(started.status, "running", "{started:?}");

        drop(service);
        thread::sleep(Duration::from_millis(700));
        assert!(
            !marker.exists(),
            "background child survived service shutdown"
        );
    }

    fn wait_for_status(registry: &TaskRegistry, task_id: &str, expected: TaskStatus) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = registry.get(task_id).map(|record| record.status);
            if status == Some(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "task {task_id} did not reach {expected:?}; observed {status:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
