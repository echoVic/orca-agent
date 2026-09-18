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
use crate::network_proxy::{
    RuntimeNetworkBlockDecision, RuntimeNetworkBlockReport, RuntimeNetworkBlockRequest,
    RuntimeNetworkPolicy, RuntimeNetworkProxy, runtime_network_permission_gate_channel,
};
#[cfg(test)]
use crate::shell_session::ShellSandboxMode;
use crate::shell_session::{
    RuntimeShellSessionManager, ShellSessionCommand, ShellSessionHandle, ShellSessionOutput,
    ShellSessionTermination, ShellTerminalMode,
};
use crate::tasks::TaskRegistry;

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const TERMINAL_COMMAND_CAPACITY: usize = 32;
const COMPLETION_QUEUE_CAPACITY: usize = 64;
const COMPLETION_OUTPUT_MAX_BYTES: usize = 8 * 1024;
const COMPLETED_SESSION_RETENTION: Duration = Duration::from_secs(10 * 60);
const MAX_COMPLETED_SESSIONS: usize = 256;

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
    /// Absolute instant the managed process must not outlive.
    deadline: Option<(Instant, &'static str)>,
    /// The configured limit this deadline was derived from, for reporting.
    deadline_after: Option<Duration>,
    requested_terminal: ShellTerminalMode,
    effective_terminal: ShellTerminalMode,
    terminal: Option<TerminalState>,
    background_notifiable: bool,
    completion_observed: bool,
    completion_queued: bool,
    completed_at: Option<Instant>,
    network_proxy: Option<RuntimeNetworkProxy>,
    network_block_receiver: Option<Receiver<RuntimeNetworkBlockRequest>>,
    pending_network_block: Option<RuntimeNetworkBlockRequest>,
}

#[derive(Clone, Copy)]
struct TerminalState {
    status: TaskStatus,
    termination: &'static str,
    exit_code: Option<i32>,
    /// Whether this terminal was produced by an execution deadline.
    deadline_reached: bool,
    deadline_source: Option<&'static str>,
}

enum TerminalCommand {
    Start {
        lifetime: crate::tasks::TaskLifetime,
        command: Box<ShellSessionCommand>,
        metadata_writable_directories: Vec<PathBuf>,
        network_proxy: Option<RuntimeNetworkProxy>,
        network_block_receiver: Option<Receiver<RuntimeNetworkBlockRequest>>,
        /// Absolute deadline armed when the process actually starts.
        deadline: Option<(Instant, &'static str)>,
        /// The configured limit this deadline was derived from.
        deadline_after: Option<Duration>,
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
        output_offset: Option<usize>,
        response: SyncSender<io::Result<TerminalServiceOutput>>,
    },
    MarkBackground {
        session_id: String,
    },
    ResolveTask {
        task_id: String,
        response: SyncSender<io::Result<String>>,
    },
    CloseInput {
        session_id: String,
        response: SyncSender<io::Result<()>>,
    },
    ResolveNetworkBlock {
        session_id: String,
        decision: RuntimeNetworkBlockDecision,
        response: SyncSender<io::Result<()>>,
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
    /// Ownership of the started command. `Workspace` marks a service the user
    /// asked to keep running; it survives its starting task.
    pub(crate) lifetime: crate::tasks::TaskLifetime,
    pub(crate) command: &'a str,
    pub(crate) cwd: &'a Path,
    pub(crate) additional_roots: &'a [PathBuf],
    pub(crate) config: &'a RunConfig,
    pub(crate) permission_overlay: &'a TurnPermissionOverlay,
    pub(crate) terminal: ShellTerminalMode,
    /// Caller-requested execution deadline, measured from process start.
    ///
    /// This is the "how long may the process run" axis, separate from the
    /// yield time that only bounds how long a tool call waits before handing
    /// control back. `None` adds no limit of its own.
    pub(crate) execution_deadline: Option<ExecutionDeadline>,
    #[cfg(test)]
    pub(crate) sandbox_override: Option<ShellSandboxMode>,
}

/// A wall-clock execution limit and where it came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecutionDeadline {
    pub(crate) after: Duration,
    pub(crate) source: &'static str,
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
    pub(crate) output_offset: usize,
    pub(crate) next_output_offset: usize,
    pub(crate) output_bytes_total: usize,
    pub(crate) eof: bool,
    pub(crate) requested_terminal: &'static str,
    pub(crate) effective_terminal: &'static str,
    /// Whether the effective deadline was reached.
    pub(crate) deadline_reached: bool,
    /// Why a deadline existed, so a caller timeout is distinguishable from an
    /// administrator cap.
    pub(crate) deadline_source: Option<&'static str>,
    /// Wall-clock milliseconds from process start to the effective deadline.
    pub(crate) effective_deadline_ms: Option<u64>,
    #[serde(skip)]
    pub(crate) network_block: Option<RuntimeNetworkBlockReport>,
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

    /// Starts one command and observes it for at most `yield_time`.
    ///
    /// `on_output` receives output as it is observed, before this call
    /// returns, so a client can render progress while the command is still
    /// running. It never changes the command.
    pub(crate) fn exec(
        &self,
        request: TerminalExecRequest<'_>,
        yield_time: Duration,
        max_output_bytes: usize,
        should_cancel: impl Fn() -> bool,
        on_output: &mut dyn FnMut(&str),
    ) -> io::Result<TerminalServiceOutput> {
        let requested_deadline = request.execution_deadline;
        let request_lifetime = request.lifetime;
        let (command, metadata_writable_directories, network_proxy, network_block_receiver) =
            prepare_shell_command(request)?;
        let deadline =
            requested_deadline.map(|deadline| (Instant::now() + deadline.after, deadline.source));
        let deadline_after = requested_deadline.map(|deadline| deadline.after);
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(TerminalCommand::Start {
            lifetime: request_lifetime,
            command: Box::new(command),
            metadata_writable_directories,
            network_proxy,
            network_block_receiver,
            deadline,
            deadline_after,
            response,
        })?;
        let handle = receive_response(receiver, "terminal start")??;
        let output = self.poll_until_with_output(
            &handle.id,
            yield_time,
            max_output_bytes,
            false,
            should_cancel,
            on_output,
        )?;
        if output.status == "running" && output.network_block.is_none() {
            let _ = self.send(TerminalCommand::MarkBackground {
                session_id: handle.id,
            });
        }
        Ok(output)
    }

    /// Resolves a model-facing `task_id` to its terminal session id.
    ///
    /// The model addresses work by `task_id` only; `session_id` stays an
    /// internal detail so callers never translate between two ids.
    fn session_id_for_task(&self, task_id: &str) -> io::Result<String> {
        // Only the supervisor owns the session map, so the lookup always goes
        // through it rather than through a second, possibly stale copy.
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(TerminalCommand::ResolveTask {
            task_id: task_id.to_string(),
            response,
        })?;
        receive_response(receiver, "terminal resolve")?
    }

    /// Reads the task's output from its own advancing cursor.
    pub(crate) fn read_output(
        &self,
        task_id: &str,
        max_output_bytes: usize,
    ) -> io::Result<Option<TerminalServiceOutput>> {
        self.read_output_at_or_cursor(task_id, None, max_output_bytes)
    }

    /// Reads one idempotent page at an absolute byte offset.
    ///
    /// The offset is caller-owned: it does not advance the task cursor, so
    /// repeated reads return the same bytes and concurrent readers never
    /// consume each other's output. An offset past the retained end is an
    /// explicit error instead of a silent restart from zero.
    pub(crate) fn read_output_at(
        &self,
        task_id: &str,
        cursor: usize,
        max_output_bytes: usize,
    ) -> io::Result<Option<TerminalServiceOutput>> {
        self.read_output_at_or_cursor(task_id, Some(cursor), max_output_bytes)
    }

    fn read_output_at_or_cursor(
        &self,
        task_id: &str,
        cursor: Option<usize>,
        max_output_bytes: usize,
    ) -> io::Result<Option<TerminalServiceOutput>> {
        let session_id = match self.session_id_for_task(task_id) {
            Ok(session_id) => session_id,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(TerminalCommand::Poll {
            session_id,
            max_output_bytes,
            output_offset: cursor,
            response,
        })?;
        Ok(Some(receive_response(receiver, "terminal read")??))
    }

    /// Writes to a task's stdin, optionally closing it, and observes output.
    pub(crate) fn send_input(
        &self,
        task_id: &str,
        chars: &str,
        eof: bool,
        yield_time: Duration,
        max_output_bytes: usize,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<Option<TerminalServiceOutput>> {
        let session_id = match self.session_id_for_task(task_id) {
            Ok(session_id) => session_id,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !chars.is_empty() {
            let (response, receiver) = mpsc::sync_channel(1);
            self.send(TerminalCommand::Write {
                session_id: session_id.clone(),
                chars: chars.to_string(),
                response,
            })?;
            receive_response(receiver, "terminal write")??;
        }
        if eof {
            let (response, receiver) = mpsc::sync_channel(1);
            self.send(TerminalCommand::CloseInput {
                session_id: session_id.clone(),
                response,
            })?;
            // A terminal without a stdin pipe cannot be closed; that must not
            // fail the write that already succeeded.
            let _ = receive_response(receiver, "terminal close input");
        }
        self.poll_until(
            &session_id,
            yield_time,
            max_output_bytes,
            true,
            should_cancel,
        )
        .map(Some)
    }

    pub(crate) fn stop_task(&self, task_id: &str) -> io::Result<bool> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(TerminalCommand::StopTask {
            task_id: task_id.to_string(),
            response,
        })?;
        receive_response(receiver, "terminal stop")?
    }

    pub(crate) fn resolve_network_permission(
        &self,
        session_id: &str,
        decision: RuntimeNetworkBlockDecision,
    ) -> io::Result<()> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(TerminalCommand::ResolveNetworkBlock {
            session_id: session_id.to_string(),
            decision,
            response,
        })?;
        receive_response(receiver, "terminal network permission")?
    }

    pub(crate) fn continue_session(
        &self,
        session_id: &str,
        yield_time: Duration,
        max_output_bytes: usize,
        should_cancel: impl Fn() -> bool,
        on_output: &mut dyn FnMut(&str),
    ) -> io::Result<TerminalServiceOutput> {
        self.poll_until_with_output(
            session_id,
            yield_time,
            max_output_bytes,
            false,
            should_cancel,
            on_output,
        )
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
        self.poll_until_with_output(
            session_id,
            yield_time,
            max_output_bytes,
            return_on_output,
            should_cancel,
            &mut |_chunk: &str| {},
        )
    }

    fn poll_until_with_output(
        &self,
        session_id: &str,
        yield_time: Duration,
        max_output_bytes: usize,
        return_on_output: bool,
        should_cancel: impl Fn() -> bool,
        on_output: &mut dyn FnMut(&str),
    ) -> io::Result<TerminalServiceOutput> {
        let deadline = Instant::now()
            .checked_add(yield_time)
            .unwrap_or_else(Instant::now);
        let mut aggregate: Option<TerminalServiceOutput> = None;
        let mut remaining_output_bytes = max_output_bytes.max(1);
        loop {
            let output = self.poll_once(session_id, remaining_output_bytes, None)?;
            let observed_output = output.output.len();
            let status = output.status;
            let network_blocked = output.network_block.is_some();
            if observed_output > 0 {
                on_output(&output.output);
            }
            merge_terminal_output(&mut aggregate, output);
            remaining_output_bytes = remaining_output_bytes.saturating_sub(observed_output);
            if status != "running" {
                return Ok(aggregate.expect("terminal poll always produces output metadata"));
            }
            if should_cancel() {
                let task_id = aggregate
                    .as_ref()
                    .expect("terminal poll always produces output metadata")
                    .task_id
                    .clone();
                let _ = self.stop_task(&task_id)?;
                let terminal = self.poll_once(session_id, remaining_output_bytes.max(1), None)?;
                merge_terminal_output(&mut aggregate, terminal);
                if let Some(output) = aggregate.as_mut() {
                    output.network_block = None;
                }
                return Ok(aggregate.expect("terminal cancellation produces output metadata"));
            }
            if network_blocked
                || (return_on_output
                    && aggregate
                        .as_ref()
                        .is_some_and(|output| !output.output.is_empty()))
                || remaining_output_bytes == 0
                || Instant::now() >= deadline
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
        output_offset: Option<usize>,
    ) -> io::Result<TerminalServiceOutput> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(TerminalCommand::Poll {
            session_id: session_id.to_string(),
            max_output_bytes,
            output_offset,
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
                lifetime,
                command,
                metadata_writable_directories,
                network_proxy,
                network_block_receiver,
                deadline,
                deadline_after,
                response,
            }) => {
                let result = state
                    .manager
                    .spawn_with_metadata_roots(*command, metadata_writable_directories);
                if let Ok(handle) = &result {
                    if lifetime == crate::tasks::TaskLifetime::Workspace
                        && !state.manager.mark_task_lifetime(&handle.task_id, lifetime)
                    {
                        // Ownership must be recorded before the caller is told
                        // the service started; otherwise a later cancel could
                        // sweep a service the user asked to keep running.
                        let _ = state.manager.kill_preserving_output(&handle.id);
                        let _ = response.send(Err(io::Error::other(
                            "failed to record workspace ownership for the started service",
                        )));
                        continue;
                    }
                    let mut session = TerminalSessionState::from_handle(
                        handle,
                        network_proxy,
                        network_block_receiver,
                    );
                    session.deadline = deadline;
                    session.deadline_after = deadline_after;
                    state.sessions.insert(handle.id.clone(), session);
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
                output_offset,
                response,
            }) => {
                let result = state.poll(&session_id, max_output_bytes, output_offset);
                let _ = response.send(result);
            }
            Ok(TerminalCommand::MarkBackground { session_id }) => {
                state.mark_background(&session_id);
            }
            Ok(TerminalCommand::ResolveTask { task_id, response }) => {
                let resolved = state
                    .sessions
                    .iter()
                    .find(|(_, session)| session.task_id == task_id)
                    .map(|(session_id, _)| session_id.clone())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("unknown terminal task: {task_id}"),
                        )
                    });
                let _ = response.send(resolved);
            }
            Ok(TerminalCommand::CloseInput {
                session_id,
                response,
            }) => {
                let result = state.close_input(&session_id);
                let _ = response.send(result);
            }
            Ok(TerminalCommand::ResolveNetworkBlock {
                session_id,
                decision,
                response,
            }) => {
                let result = state.resolve_network_permission(&session_id, decision);
                let _ = response.send(result);
            }
            Ok(TerminalCommand::StopTask { task_id, response }) => {
                let result = state.stop_task(&task_id);
                let _ = response.send(result);
            }
            Ok(TerminalCommand::DrainCompletions { response }) => {
                let _ = response.send(state.completions.drain(..).collect());
            }
            Ok(TerminalCommand::Shutdown { response }) => {
                state.deny_pending_network_requests();
                state.manager.terminate_all();
                let _ = response.send(());
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                state.deny_pending_network_requests();
                state.manager.terminate_all();
                break;
            }
        }
        let _ = state.enforce_deadlines();
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
        output_offset: Option<usize>,
    ) -> io::Result<TerminalServiceOutput> {
        // Do not call reap() here: joining output reader threads and persisting
        // task-registry records is slow enough to block the supervisor for
        // hundreds of milliseconds, which causes poll_once() to blow past the
        // caller's yield_time deadline.  The maintenance loop after each command
        // already runs reap(), so completed-session bookkeeping happens in the
        // background without delaying Poll responses.
        let (task_id, cursor, requested_terminal, effective_terminal, terminal) =
            if let Some(session) = self.sessions.get(session_id) {
                (
                    session.task_id.clone(),
                    session.cursor,
                    session.requested_terminal,
                    session.effective_terminal,
                    session.terminal,
                )
            } else {
                let archived = self.manager.output_store().archived_shell(session_id)?;
                let terminal = TerminalState::from_archive(&archived)?;
                (
                    archived.task_id,
                    archived.cursor,
                    if archived.requested_pty {
                        ShellTerminalMode::pty(None, None)
                    } else {
                        ShellTerminalMode::pipe()
                    },
                    if archived.effective_pty {
                        ShellTerminalMode::pty(None, None)
                    } else {
                        ShellTerminalMode::pipe()
                    },
                    Some(terminal),
                )
            };
        let cursor = output_offset.unwrap_or(cursor);
        let output =
            self.manager
                .output_store()
                .read_delta(&task_id, cursor, max_output_bytes.max(1))?;
        let terminal = terminal.unwrap_or_else(TerminalState::running);
        if output_offset.is_none() {
            if output.next_offset != cursor {
                self.manager
                    .output_store()
                    .set_cursor(session_id, output.next_offset)?;
            }
            if let Some(session) = self.sessions.get_mut(session_id) {
                session.cursor = output.next_offset;
            }
            if terminal.status != TaskStatus::Running {
                self.observe_completion(session_id);
            }
            // A session that reached EOF stays addressable until the retention
            // window expires. Dropping it here made a finished command
            // unreadable by its task_id: the model could see the completion
            // notification but could not page the output it referred to.
            let _ = &task_id;
        }
        // A deadline stop is recorded on the session's terminal state, but the
        // command may have been observed terminal before that stamp existed
        // (the supervisor's next maintenance tick). The session's own deadline
        // is the tiebreaker: if it has expired and the command is terminal,
        // the deadline is what ended it.
        let deadline_expired = self
            .sessions
            .get(session_id)
            .and_then(|session| session.deadline)
            .is_some_and(|(deadline, _source)| Instant::now() >= deadline);
        let deadline_reached = terminal.deadline_reached
            || (deadline_expired && terminal.status != TaskStatus::Running);
        let deadline_source = terminal.deadline_source.or_else(|| {
            self.sessions
                .get(session_id)
                .and_then(|session| session.deadline)
                .map(|(_deadline, source)| source)
        });
        let network_block = self.sessions.get_mut(session_id).and_then(|session| {
            if session.pending_network_block.is_none() {
                session.pending_network_block = session
                    .network_block_receiver
                    .as_ref()
                    .and_then(|receiver| receiver.try_recv().ok());
            }
            session
                .pending_network_block
                .as_ref()
                .map(|request| request.report.clone())
        });
        Ok(TerminalServiceOutput {
            session_id: session_id.to_string(),
            task_id,
            status: if terminal.termination == "interrupted" {
                "interrupted"
            } else {
                task_status_label(terminal.status)
            },
            termination: if deadline_reached {
                "timed_out"
            } else {
                terminal.termination
            },
            output: output.combined,
            exit_code: terminal.exit_code,
            truncated: output.omitted_prefix_bytes > 0 || output.next_offset < output.bytes_total,
            omitted_prefix_bytes: output.omitted_prefix_bytes,
            output_offset: cursor,
            next_output_offset: output.next_offset,
            output_bytes_total: output.bytes_total,
            eof: terminal.status != TaskStatus::Running && output.next_offset >= output.bytes_total,
            requested_terminal: requested_terminal.as_str(),
            effective_terminal: effective_terminal.as_str(),
            deadline_reached,
            deadline_source,
            effective_deadline_ms: self
                .sessions
                .get(session_id)
                .and_then(|session| session.deadline_after)
                .map(|after| after.as_millis() as u64),
            network_block,
        })
    }

    /// Terminates every managed process whose caller deadline has expired.
    ///
    /// A deadline limits execution, not waiting: the process tree is stopped
    /// and recorded as timed out, with partial output preserved.
    fn enforce_deadlines(&mut self) -> io::Result<()> {
        let now = Instant::now();
        let expired = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.terminal.is_none()
                    && session
                        .deadline
                        .is_some_and(|(deadline, _source)| now >= deadline)
            })
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in expired {
            let source = self
                .sessions
                .get(&session_id)
                .and_then(|session| session.deadline)
                .map(|(_deadline, source)| source);
            let mut output = self.manager.stop_for_deadline(&session_id)?;
            output.status = TaskStatus::Failed;
            // Stamp the terminal before it is recorded: the recorded state is
            // what every later poll reports, and the first response the caller
            // sees must already name the deadline that stopped the command.
            self.record_terminal_with_deadline(&output, source);
        }
        Ok(())
    }

    fn close_input(&mut self, session_id: &str) -> io::Result<()> {
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
        self.manager.close_stdin(session_id)
    }

    fn resolve_network_permission(
        &mut self,
        session_id: &str,
        decision: RuntimeNetworkBlockDecision,
    ) -> io::Result<()> {
        let session = self.sessions.get_mut(session_id).ok_or_else(|| {
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
        let request = session.pending_network_block.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("terminal session has no pending network permission: {session_id}"),
            )
        })?;
        request.resolve(decision)
    }

    fn deny_pending_network_requests(&mut self) {
        for session in self.sessions.values_mut() {
            if let Some(request) = session.pending_network_block.take() {
                let _ = request.resolve(RuntimeNetworkBlockDecision::Deny);
            }
        }
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
        self.record_terminal_with_deadline(output, None);
    }

    fn record_terminal_with_deadline(
        &mut self,
        output: &ShellSessionOutput,
        deadline_source: Option<&'static str>,
    ) {
        if let Some(session) = self.sessions.get_mut(&output.id)
            && session.terminal.is_none()
        {
            let mut terminal = TerminalState::from_output(output);
            if let Some(source) = deadline_source {
                terminal.deadline_reached = true;
                terminal.deadline_source = Some(source);
            }
            session.terminal = Some(terminal);
            session.completed_at = Some(Instant::now());
            if let Some(request) = session.pending_network_block.take() {
                let _ = request.resolve(RuntimeNetworkBlockDecision::Deny);
            }
            session.network_block_receiver.take();
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
        let output =
            self.manager
                .output_store()
                .read_delta(&task_id, 0, COMPLETION_OUTPUT_MAX_BYTES);
        let completion = TerminalCompletion {
            session_id: session_id.to_string(),
            task_id,
            status: task_status_label(terminal.status),
            output: output
                .as_ref()
                .map(|output| output.combined.clone())
                .unwrap_or_else(|error| format!("[task output archive unavailable: {error}]")),
            exit_code: terminal.exit_code,
            truncated: output.as_ref().map_or(true, |output| {
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
        let mut completed = self
            .sessions
            .iter()
            .filter_map(|(session_id, session)| {
                session
                    .completed_at
                    .map(|at| (at, session_id.clone(), session.task_id.clone()))
            })
            .collect::<Vec<_>>();
        completed.sort_by_key(|(at, _, _)| *at);
        let excess = completed.len().saturating_sub(MAX_COMPLETED_SESSIONS);
        for (index, (at, session_id, task_id)) in completed.into_iter().enumerate() {
            if index < excess || at.elapsed() >= COMPLETED_SESSION_RETENTION {
                self.sessions.remove(&session_id);
                self.manager.remove_output(&task_id);
            }
        }
    }
}

pub(crate) fn merge_terminal_output(
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
    current.eof = next.eof;
    current.requested_terminal = next.requested_terminal;
    current.effective_terminal = next.effective_terminal;
    current.network_block = current.network_block.take().or(next.network_block);
}

impl TerminalSessionState {
    fn from_handle(
        handle: &ShellSessionHandle,
        network_proxy: Option<RuntimeNetworkProxy>,
        network_block_receiver: Option<Receiver<RuntimeNetworkBlockRequest>>,
    ) -> Self {
        Self {
            task_id: handle.task_id.clone(),
            cursor: 0,
            deadline: None,
            deadline_after: None,
            requested_terminal: handle.requested_terminal,
            effective_terminal: handle.effective_terminal,
            terminal: None,
            background_notifiable: false,
            completion_observed: false,
            completion_queued: false,
            completed_at: None,
            network_proxy,
            network_block_receiver,
            pending_network_block: None,
        }
    }
}

impl TerminalState {
    fn running() -> Self {
        Self {
            deadline_reached: false,
            deadline_source: None,
            status: TaskStatus::Running,
            termination: "running",
            exit_code: None,
        }
    }

    fn from_output(output: &ShellSessionOutput) -> Self {
        Self {
            status: output.status,
            termination: termination_label(output.termination),
            exit_code: output.exit_code,
            deadline_reached: output.termination == ShellSessionTermination::TimedOut,
            deadline_source: None,
        }
    }

    fn from_archive(output: &crate::task_output::ArchivedShell) -> io::Result<Self> {
        let (status, termination) = match output.state.as_str() {
            "running" => (TaskStatus::Running, "running"),
            "exited" => (
                if output.exit_code == Some(0) {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                },
                "exited",
            ),
            "cancelled" => (TaskStatus::Stopped, "cancelled"),
            "timed_out" => (TaskStatus::Stopped, "timed_out"),
            "interrupted" => (TaskStatus::Failed, "interrupted"),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid archived terminal state",
                ));
            }
        };
        Ok(Self {
            status,
            termination,
            exit_code: output.exit_code,
            // An archived record does not persist the deadline origin, so a
            // recovered timeout reports the fact without inventing a source.
            deadline_reached: termination == "timed_out",
            deadline_source: None,
        })
    }
}

fn prepare_shell_command(
    request: TerminalExecRequest<'_>,
) -> io::Result<(
    ShellSessionCommand,
    Vec<PathBuf>,
    Option<RuntimeNetworkProxy>,
    Option<Receiver<RuntimeNetworkBlockRequest>>,
)> {
    let mut sandbox = crate::server::bash_sandbox_for_cwd(request.config, request.cwd)
        .map_err(io::Error::other)?;
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

    let (network_proxy, network_block_receiver) = if sandbox.network_policy_domains.is_empty() {
        (None, None)
    } else {
        let (permission_gate, receiver) = runtime_network_permission_gate_channel();
        (
            Some(RuntimeNetworkProxy::start_with_permission_gate(
                RuntimeNetworkPolicy::new(sandbox.network_policy_domains.clone()),
                Some(permission_gate),
            )?),
            Some(receiver),
        )
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
        network_block_receiver,
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

    fn host_long_running_command() -> &'static str {
        match orca_platform::shell::ShellResolver::for_current_host()
            .resolve_from_environment()
            .expect("resolve host shell")
            .kind()
        {
            orca_platform::shell::ShellKind::Posix | orca_platform::shell::ShellKind::GitBash => {
                "printf ready; sleep 30"
            }
            orca_platform::shell::ShellKind::PowerShell(_) => {
                "Write-Host -NoNewline 'ready'; Start-Sleep -Seconds 30"
            }
            orca_platform::shell::ShellKind::Cmd => "echo ready & ping 127.0.0.1 -n 31 > nul",
        }
    }

    fn service(cwd: &Path) -> (TerminalService, TaskRegistry) {
        let registry = TaskRegistry::new_persistent(
            format!("terminal-test-{}", uuid::Uuid::new_v4()),
            cwd.join("tasks"),
        )
        .expect("isolated task registry");
        (TerminalService::new(registry.clone()), registry)
    }

    /// Test-only polling helpers for this module's supervisor tests.
    ///
    /// The model-facing path is `send_input` / `read_output`, which address a
    /// task by `task_id` and never expose a raw session id. These wrappers keep
    /// the original session-addressed shape so the supervisor tests stay
    /// focused on lifecycle and archive behaviour.
    impl TerminalService {
        pub(crate) fn write_stdin(
            &self,
            session_id: &str,
            chars: Option<&str>,
            yield_time: Duration,
            max_output_bytes: usize,
            should_cancel: impl Fn() -> bool,
        ) -> io::Result<TerminalServiceOutput> {
            self.write_stdin_with_offset(
                session_id,
                chars,
                None,
                yield_time,
                max_output_bytes,
                should_cancel,
            )
        }

        pub(crate) fn write_stdin_with_offset(
            &self,
            session_id: &str,
            chars: Option<&str>,
            output_offset: Option<usize>,
            yield_time: Duration,
            max_output_bytes: usize,
            should_cancel: impl Fn() -> bool,
        ) -> io::Result<TerminalServiceOutput> {
            if let Some(offset) = output_offset {
                if chars.is_some_and(|chars| !chars.is_empty()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "output_offset cannot be combined with nonempty chars",
                    ));
                }
                return self.poll_once(session_id, max_output_bytes, Some(offset));
            }
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
    }

    /// Test wrapper: these suites observe no incremental output.
    fn start(
        service: &TerminalService,
        request: TerminalExecRequest<'_>,
        yield_time: Duration,
        max_output_bytes: usize,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<TerminalServiceOutput> {
        service.exec(
            request,
            yield_time,
            max_output_bytes,
            should_cancel,
            &mut |_chunk: &str| {},
        )
    }

    fn request_with_deadline<'a>(
        command: &'a str,
        cwd: &'a Path,
        overlay: &'a TurnPermissionOverlay,
        deadline: Option<ExecutionDeadline>,
    ) -> TerminalExecRequest<'a> {
        let mut request = request(command, cwd, overlay, ShellTerminalMode::pipe());
        request.execution_deadline = deadline;
        request
    }

    fn request<'a>(
        command: &'a str,
        cwd: &'a Path,
        overlay: &'a TurnPermissionOverlay,
        terminal: ShellTerminalMode,
    ) -> TerminalExecRequest<'a> {
        let config = Box::leak(Box::new(
            crate::command::config::assemble_run_config(
                crate::command::config::RunConfigRequest::new("0.0.0-test", cwd.to_path_buf()),
                orca_core::config::file::FileConfig::default(),
            )
            .expect("test config"),
        ));
        TerminalExecRequest {
            lifetime: crate::tasks::TaskLifetime::Task,
            command,
            cwd,
            additional_roots: &[],
            config,
            permission_overlay: overlay,
            terminal,
            execution_deadline: None,
            sandbox_override: Some(ShellSandboxMode::DangerFullAccess),
        }
    }

    #[test]
    fn exec_returns_completed_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let overlay = TurnPermissionOverlay::default();
        let (service, _) = service(temp.path());
        let output = start(
            &service,
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

    #[cfg(not(windows))]
    #[test]
    fn exec_returns_structured_network_block_receipt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let upstream = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind upstream");
        let upstream_port = upstream.local_addr().expect("upstream address").port();
        let server = std::thread::spawn(move || {
            use std::io::{BufRead, Write};

            let (mut stream, _) = upstream.accept().expect("accept resumed request");
            let mut reader =
                std::io::BufReader::new(stream.try_clone().expect("clone upstream stream"));
            let mut line = String::new();
            while reader.read_line(&mut line).expect("read request") != 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\nresumed")
                .expect("write upstream response");
        });
        let mut overlay = TurnPermissionOverlay::default();
        overlay.merge_network_permissions(&crate::protocol::RequestPermissionProfile {
            file_system: None,
            network: Some(crate::protocol::RequestNetworkPermissions {
                enabled: Some(true),
                domains: std::collections::HashMap::from([(
                    "api.example.com".to_string(),
                    PermissionProfileNetworkAccess::Allow,
                )]),
            }),
        });
        let (service, _) = service(temp.path());
        let output = start(
            &service,
            request(
                &format!(
                    "curl --max-time 5 --proxy \"$HTTP_PROXY\" -sS http://127.0.0.1:{upstream_port}/"
                ),
                temp.path(),
                &overlay,
                ShellTerminalMode::pipe(),
            ),
            Duration::from_secs(5),
            8 * 1024,
            || false,
        )
        .expect("exec through policy proxy");

        assert_eq!(output.status, "running");
        assert_eq!(
            output.network_block,
            Some(RuntimeNetworkBlockReport {
                host: "127.0.0.1".to_string(),
                error: "blocked-by-policy",
            })
        );
        service
            .resolve_network_permission(&output.session_id, RuntimeNetworkBlockDecision::Allow)
            .expect("allow blocked connection");
        let completed = service
            .write_stdin(
                &output.session_id,
                None,
                Duration::from_secs(5),
                8 * 1024,
                || false,
            )
            .expect("poll resumed command");
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.output, "resumed");
        server.join().expect("upstream server");
    }

    #[test]
    fn running_session_accepts_stdin() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, _) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let command = if cfg!(windows) {
            r#"$line = [Console]::In.ReadLine(); [Console]::Out.Write("got:$line")"#
        } else {
            "read line; printf 'got:%s' \"$line\""
        };
        let started = start(
            &service,
            request(command, temp.path(), &overlay, ShellTerminalMode::pipe()),
            Duration::from_millis(50),
            8 * 1024,
            || false,
        )
        .expect("start");
        assert_eq!(started.status, "running", "{started:?}");

        let observed = service
            .write_stdin(
                &started.session_id,
                Some(if cfg!(windows) {
                    "hello\r\n"
                } else {
                    "hello\n"
                }),
                Duration::from_secs(2),
                8 * 1024,
                || false,
            )
            .expect("write stdin");
        let mut output = observed.output.clone();
        let completed = if observed.status == "running" {
            service
                .write_stdin(
                    &started.session_id,
                    None,
                    Duration::from_secs(5),
                    8 * 1024,
                    || false,
                )
                .expect("poll stdin command completion")
        } else {
            observed
        };
        output.push_str(&completed.output);
        assert_eq!(completed.status, "completed", "{completed:?}");
        assert!(output.contains("got:hello"), "{output:?}");
    }

    #[cfg(unix)]
    #[test]
    fn pty_session_applies_ctrl_u_line_kill() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, _) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let started = start(
            &service,
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
        let command = host_long_running_command();
        let started = start(
            &service,
            request(command, temp.path(), &overlay, ShellTerminalMode::pipe()),
            Duration::from_millis(50),
            8 * 1024,
            || false,
        )
        .expect("start long command");
        assert_eq!(started.status, "running", "{started:?}");
        let stop_started = Instant::now();
        assert!(service.stop_task(&started.task_id).expect("stop task"));
        let stopped = service
            .write_stdin(&started.session_id, None, Duration::ZERO, 8 * 1024, || {
                false
            })
            .expect("poll stopped task");
        assert!(
            stop_started.elapsed() < Duration::from_secs(10),
            "stopping a command must remain bounded well below natural completion"
        );

        assert_ne!(stopped.status, "running", "{stopped:?}");
        assert_eq!(stopped.termination, "cancelled", "{stopped:?}");
    }

    #[test]
    fn exec_cancellation_stops_the_running_process_before_returning() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, _) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();

        let output = start(
            &service,
            request(
                host_long_running_command(),
                temp.path(),
                &overlay,
                ShellTerminalMode::pipe(),
            ),
            Duration::from_secs(5),
            8 * 1024,
            || true,
        )
        .expect("cancel long command");

        assert_eq!(output.status, "stopped", "{output:?}");
        assert_eq!(output.termination, "cancelled", "{output:?}");
    }

    #[test]
    fn background_command_settles_without_poll() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (service, registry) = service(temp.path());
        let overlay = TurnPermissionOverlay::default();
        let started = start(
            &service,
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
        let command = host_long_running_command();
        let started = start(
            &service,
            request(command, temp.path(), &overlay, ShellTerminalMode::pipe()),
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
        let started = start(
            &service,
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
        let started = start(
            &service,
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
        let first = start(
            &service,
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
        let second = start(
            &service,
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
        let started = start(
            &service,
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

    fn explicit_page(
        service: &TerminalService,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> TerminalServiceOutput {
        service
            .write_stdin_with_offset(id, None, Some(offset), Duration::ZERO, limit, || false)
            .unwrap()
    }

    #[test]
    fn explicit_pages_replay_after_eof_and_service_restart() {
        let temp = tempfile::tempdir().unwrap();
        let overlay = TurnPermissionOverlay::default();
        let (service, registry) = service(temp.path());
        let command = if cfg!(windows) {
            "[Console]::Out.Write('abcdef')"
        } else {
            "printf abcdef"
        };
        let started = start(
            &service,
            request(command, temp.path(), &overlay, ShellTerminalMode::pipe()),
            Duration::from_secs(2),
            2,
            || false,
        )
        .unwrap();
        wait_for_status(&registry, &started.task_id, TaskStatus::Completed);
        let first = explicit_page(&service, &started.session_id, 0, 2);
        assert_eq!(first.output, "ab");
        assert_eq!(first.next_output_offset, 2);
        assert!(!first.eof);
        assert_eq!(first, explicit_page(&service, &started.session_id, 0, 2));
        let next = explicit_page(&service, &started.session_id, first.next_output_offset, 2);
        assert_eq!(next.output, "cd");
        let automatic = service
            .write_stdin(&started.session_id, None, Duration::ZERO, 99, || false)
            .unwrap();
        assert_eq!(
            automatic.output, "cdef",
            "explicit reads must not consume the automatic cursor"
        );
        assert!(automatic.eof);
        assert_eq!(
            explicit_page(&service, &started.session_id, 0, 6).output,
            "abcdef"
        );
        let eof = explicit_page(&service, &started.session_id, 6, 10);
        assert!(eof.eof && eof.output.is_empty());
        assert_eq!(eof, explicit_page(&service, &started.session_id, 6, 10));
        assert_eq!(
            service
                .write_stdin_with_offset(
                    &started.session_id,
                    None,
                    Some(7),
                    Duration::ZERO,
                    5,
                    || false
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput,
        );
        drop(service);
        let reopened_registry = TaskRegistry::new_persistent(
            registry.session_id().to_string(),
            temp.path().join("tasks"),
        )
        .unwrap();
        let reopened = TerminalService::new(reopened_registry);
        assert_eq!(first, explicit_page(&reopened, &started.session_id, 0, 2));
        assert!(
            reopened
                .write_stdin(&started.session_id, None, Duration::ZERO, 99, || false)
                .unwrap()
                .output
                .is_empty()
        );
        assert!(
            reopened
                .write_stdin(
                    &started.session_id,
                    Some("input"),
                    Duration::ZERO,
                    99,
                    || false
                )
                .is_err()
        );
        let foreign = TerminalService::new(
            TaskRegistry::new_persistent("foreign".to_string(), temp.path().join("tasks")).unwrap(),
        );
        assert!(
            foreign
                .write_stdin_with_offset(
                    &started.session_id,
                    None,
                    Some(0),
                    Duration::ZERO,
                    99,
                    || false
                )
                .is_err()
        );
    }

    #[test]
    fn live_offset_reads_cannot_write_or_advance_stdin_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let overlay = TurnPermissionOverlay::default();
        let (service, registry) = service(temp.path());
        let command = if cfg!(windows) {
            "[Console]::Out.Write('seed'); [Console]::Out.Flush(); $line = [Console]::In.ReadLine(); [Console]::Out.Write(\":$line\")"
        } else {
            "printf seed; read line; printf ':%s' \"$line\""
        };
        let started = start(
            &service,
            request(command, temp.path(), &overlay, ShellTerminalMode::pipe()),
            Duration::from_secs(1),
            99,
            || false,
        )
        .unwrap();
        assert_eq!(started.status, "running");
        assert_eq!(started.output, "seed");
        assert_eq!(
            explicit_page(&service, &started.session_id, 0, 2).output,
            "se"
        );
        let invalid = service
            .write_stdin_with_offset(
                &started.session_id,
                Some("wrong\n"),
                Some(0),
                Duration::ZERO,
                99,
                || false,
            )
            .unwrap_err();
        assert_eq!(invalid.kind(), io::ErrorKind::InvalidInput);
        let idle = service
            .write_stdin(&started.session_id, None, Duration::ZERO, 99, || false)
            .unwrap();
        assert_eq!(idle.next_output_offset, 4);
        assert!(idle.output.is_empty());
        service
            .write_stdin(
                &started.session_id,
                Some("hello\n"),
                Duration::from_secs(2),
                99,
                || false,
            )
            .unwrap();
        wait_for_status(&registry, &started.task_id, TaskStatus::Completed);
        let output = explicit_page(&service, &started.session_id, 0, 99);
        assert_eq!(output.output, "seed:hello");
        assert!(output.eof);
    }

    #[test]
    fn recovered_running_output_is_interrupted_and_storage_tampering_is_safe() {
        let temp = tempfile::tempdir().unwrap();
        let registry =
            TaskRegistry::new_persistent("recover".to_string(), temp.path().join("tasks")).unwrap();
        {
            let store = crate::task_output::TaskOutputStore::for_tasks(&registry);
            store
                .register_shell("shell-recovered", "task-recovered", false, false)
                .unwrap();
            store
                .append_stdout("task-recovered", "before-restart")
                .unwrap();
        }
        let service = TerminalService::new(registry.clone());
        let page = explicit_page(&service, "shell-recovered", 0, 99);
        assert_eq!(page.status, "interrupted");
        assert_eq!(page.termination, "interrupted");
        assert!(page.eof);
        assert_eq!(page.output, "before-restart");
        assert_eq!(page.exit_code, None);
        let database = registry
            .output_storage_root()
            .unwrap()
            .unwrap()
            .join("archive.sqlite3");
        #[cfg(unix)]
        {
            std::fs::remove_file(database).unwrap();
            assert!(
                service
                    .write_stdin_with_offset(
                        "shell-recovered",
                        None,
                        Some(0),
                        Duration::ZERO,
                        99,
                        || false
                    )
                    .is_err()
            );
        }
        #[cfg(windows)]
        {
            assert!(std::fs::remove_file(database).is_err());
            assert_eq!(
                explicit_page(&service, "shell-recovered", 0, 99).output,
                "before-restart"
            );
        }
    }

    #[test]
    fn execution_deadline_terminates_the_process_tree_and_reports_timed_out() {
        let temp = tempfile::tempdir().expect("tempdir");
        let overlay = TurnPermissionOverlay::default();
        let (service, registry) = service(temp.path());
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 60"
        } else {
            "sleep 60"
        };
        // Long enough that process startup cannot consume it, short enough
        // that the test stays fast.
        let deadline = ExecutionDeadline {
            after: Duration::from_millis(1_500),
            source: "caller timeout_ms",
        };

        let started = start(
            &service,
            request_with_deadline(command, temp.path(), &overlay, Some(deadline)),
            Duration::ZERO,
            8 * 1024,
            || false,
        )
        .expect("started");
        assert_eq!(
            started.status, "running",
            "unexpected start output: {started:?}"
        );
        assert_eq!(started.effective_deadline_ms, Some(1_500));
        assert_eq!(started.deadline_source, Some("caller timeout_ms"));

        // The process must end on its own at the deadline.
        let give_up_at = Instant::now() + Duration::from_secs(15);
        let terminal = loop {
            let observed = service
                .read_output(&started.task_id, 8 * 1024)
                .expect("read")
                .expect("known task");
            if observed.status != "running" {
                break observed;
            }
            assert!(
                Instant::now() < give_up_at,
                "the deadline did not stop the managed process"
            );
            std::thread::sleep(Duration::from_millis(25));
        };

        assert_eq!(terminal.termination, "timed_out");
        assert!(
            terminal.deadline_reached,
            "deadline_reached must survive the terminal transition: {terminal:?}"
        );
        assert_eq!(terminal.deadline_source, Some("caller timeout_ms"));
        assert_ne!(
            terminal.exit_code,
            Some(0),
            "a command stopped by its deadline must never look like a clean exit"
        );
        let recorded = registry
            .get(&started.task_id)
            .map(|task| task.status)
            .expect("task record");
        assert!(
            matches!(recorded, TaskStatus::Stopped | TaskStatus::Failed),
            "the task record must not claim the deadline-killed command completed: {recorded:?}"
        );
    }

    #[test]
    fn no_deadline_leaves_the_process_running_past_the_yield() {
        let temp = tempfile::tempdir().expect("tempdir");
        let overlay = TurnPermissionOverlay::default();
        let (service, _) = service(temp.path());
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };

        let started = start(
            &service,
            request(command, temp.path(), &overlay, ShellTerminalMode::pipe()),
            Duration::from_millis(200),
            8 * 1024,
            || false,
        )
        .expect("started");

        assert_eq!(started.status, "running");
        assert_eq!(
            started.exit_code, None,
            "a running command has no exit code to report"
        );
        assert_eq!(started.effective_deadline_ms, None);
        assert_eq!(started.deadline_source, None);

        std::thread::sleep(Duration::from_millis(400));
        let observed = service
            .read_output(&started.task_id, 8 * 1024)
            .expect("read")
            .expect("known task");
        assert_eq!(
            observed.status, "running",
            "yield elapsing must never terminate the command"
        );

        service.stop_task(&started.task_id).expect("stop");
    }

    #[test]
    fn cursor_reads_are_per_caller_and_idempotent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let overlay = TurnPermissionOverlay::default();
        let (service, _) = service(temp.path());
        let command = if cfg!(windows) {
            "[Console]::Out.Write('abcdefghij')"
        } else {
            "printf abcdefghij"
        };

        let started = start(
            &service,
            request(command, temp.path(), &overlay, ShellTerminalMode::pipe()),
            Duration::from_secs(5),
            8 * 1024,
            || false,
        )
        .expect("started");
        assert_ne!(
            started.status, "running",
            "the command should have finished"
        );
        assert!(
            service
                .read_output(&started.task_id, 16)
                .expect("lookup")
                .is_some(),
            "the finished task must remain addressable by task_id"
        );

        // Two readers asking for the same page get the same bytes: a read
        // never consumes a shared position.
        let first = service
            .read_output_at(&started.task_id, 0, 4)
            .expect("read")
            .expect("known task");
        let second = service
            .read_output_at(&started.task_id, 0, 4)
            .expect("read")
            .expect("known task");
        assert_eq!(first.output, second.output);
        assert!(!first.output.is_empty());

        let third = service
            .read_output_at(&started.task_id, first.next_output_offset, 64)
            .expect("read")
            .expect("known task");
        assert_ne!(third.output, first.output);

        // An offset past the retained end is an explicit error rather than a
        // silent restart from zero.
        assert!(
            service
                .read_output_at(&started.task_id, usize::MAX, 16)
                .is_err(),
            "an out-of-range cursor must fail loudly"
        );

        // An unknown task is reported as absent, not as empty output.
        assert!(
            service
                .read_output("cmd-does-not-exist", 16)
                .expect("lookup")
                .is_none()
        );
    }

    #[test]
    fn terminal_state_is_not_reopened_by_late_output() {
        let temp = tempfile::tempdir().expect("tempdir");
        let overlay = TurnPermissionOverlay::default();
        let (service, _) = service(temp.path());
        let command = if cfg!(windows) {
            "[Console]::Out.Write('done')"
        } else {
            "printf done"
        };

        let started = start(
            &service,
            request(command, temp.path(), &overlay, ShellTerminalMode::pipe()),
            Duration::from_secs(5),
            8 * 1024,
            || false,
        )
        .expect("started");
        assert_ne!(started.status, "running");
        let exit_code = started.exit_code;

        for _ in 0..3 {
            let observed = service
                .read_output(&started.task_id, 8 * 1024)
                .expect("read")
                .expect("known task");
            assert_ne!(
                observed.status, "running",
                "a terminal task must not return to running"
            );
            assert_eq!(observed.exit_code, exit_code);
        }
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
