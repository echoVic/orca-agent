use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
#[cfg(unix)]
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::str;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use orca_core::task_types::TaskStatus;
use orca_platform::process::ProcessJob;
#[cfg(windows)]
use orca_platform::shell::PowerShellEdition;
use orca_platform::shell::{ShellKind, ShellResolver, ShellSpec};
use orca_platform::terminal::native_pty_supported;
#[cfg(windows)]
use orca_platform::terminal::{WindowsPtyChild, WindowsPtyInput, spawn_windows_pty};
#[cfg(windows)]
use orca_windows_sandbox::{
    CapabilityStore, SandboxFilesystemMode, SandboxSpawnRequest, SandboxedChild, SandboxedPty,
    SandboxedPtyInput, WindowsSandboxPlan, WindowsSandboxPolicyInput,
};
use uuid::Uuid;

use crate::execution_broker::ExecutionBroker;
use crate::task_output::{TaskOutputRead, TaskOutputStore};
use crate::tasks::TaskRegistry;
use orca_core::capability::{
    CapabilityProcessClass, CapabilityReceipt, CapabilityRequest, CapabilitySet,
    EffectiveCapability, EnforcementState,
};

#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::os::unix::io::FromRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

#[derive(Clone, Debug)]
pub struct ShellSessionCommand {
    pub command: String,
    pub argv: Option<Vec<String>>,
    pub cwd: PathBuf,
    pub additional_readable_directories: Vec<PathBuf>,
    pub additional_working_directories: Vec<PathBuf>,
    pub denied_working_directories: Vec<PathBuf>,
    pub allowed_unix_socket_roots: Vec<PathBuf>,
    pub env: BTreeMap<String, Option<String>>,
    pub description: String,
    pub terminal: ShellTerminalMode,
    pub sandbox: ShellSandboxMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellTerminalMode {
    Pipe,
    Pty {
        cols: Option<u16>,
        rows: Option<u16>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellSandboxMode {
    WorkspaceWrite {
        network_access: bool,
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    },
    ReadOnly {
        network_access: bool,
        allow_global_read: bool,
    },
    DangerFullAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellRuntimeCapabilities {
    pub platform: &'static str,
    pub supports_pty: bool,
    pub supports_pty_resize: bool,
    pub fallback_terminal_mode: ShellTerminalMode,
    pub command_exec_streaming_requires_process_id: bool,
}

impl Default for ShellSandboxMode {
    fn default() -> Self {
        Self::WorkspaceWrite {
            network_access: true,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        }
    }
}

pub fn shell_runtime_capabilities() -> ShellRuntimeCapabilities {
    ShellRuntimeCapabilities {
        platform: shell_runtime_platform(),
        supports_pty: native_pty_supported(),
        supports_pty_resize: native_pty_supported(),
        fallback_terminal_mode: ShellTerminalMode::pipe(),
        command_exec_streaming_requires_process_id: true,
    }
}

fn shell_runtime_platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macos"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(all(
        not(target_os = "macos"),
        not(target_os = "linux"),
        not(target_os = "windows")
    ))]
    {
        std::env::consts::OS
    }
}

impl ShellTerminalMode {
    pub fn pipe() -> Self {
        Self::Pipe
    }

    pub fn pty(cols: Option<u16>, rows: Option<u16>) -> Self {
        Self::Pty { cols, rows }
    }

    pub fn is_pty(self) -> bool {
        matches!(self, Self::Pty { .. })
    }

    pub fn size(self) -> (Option<u16>, Option<u16>) {
        match self {
            Self::Pipe => (None, None),
            Self::Pty { cols, rows } => (cols, rows),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pipe => "pipe",
            Self::Pty { .. } => "pty",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellSessionHandle {
    pub id: String,
    pub task_id: String,
    pub requested_terminal: ShellTerminalMode,
    pub effective_terminal: ShellTerminalMode,
    pub capability_receipt: CapabilityReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellSessionOutput {
    pub id: String,
    pub task_id: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub status: TaskStatus,
    pub termination: ShellSessionTermination,
    pub requested_terminal: ShellTerminalMode,
    pub effective_terminal: ShellTerminalMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellSessionTermination {
    Running,
    Exited,
    Cancelled,
    TimedOut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellSessionSnapshot {
    pub id: String,
    pub task_id: String,
    pub command: String,
    pub description: String,
    pub status: TaskStatus,
    pub requested_terminal: ShellTerminalMode,
    pub effective_terminal: ShellTerminalMode,
}

pub struct RuntimeShellSessionManager {
    tasks: TaskRegistry,
    output_store: TaskOutputStore,
    sessions: HashMap<String, ShellSession>,
}

struct ShellSession {
    tasks: TaskRegistry,
    task_id: String,
    command: String,
    description: String,
    child: ShellChild,
    process_job: ProcessJob,
    stdin: ShellInput,
    output_store: TaskOutputStore,
    stdout_handle: Option<thread::JoinHandle<()>>,
    stderr_handle: Option<thread::JoinHandle<()>>,
    reader_stop: Arc<AtomicBool>,
    requested_terminal: ShellTerminalMode,
    effective_terminal: ShellTerminalMode,
    capability_receipt: CapabilityReceipt,
}

struct SpawnedShellChild {
    child: Option<ShellChild>,
}

enum ShellInput {
    Pipe(Option<ChildStdin>),
    #[cfg(windows)]
    WindowsSandbox(Option<std::fs::File>),
    #[cfg(windows)]
    WindowsSandboxPty(SandboxedPtyInput),
    #[cfg(unix)]
    UnixPty(File),
    #[cfg(windows)]
    WindowsPty(WindowsPtyInput),
}

enum ShellChild {
    Process(Child),
    #[cfg(windows)]
    WindowsSandbox(SandboxedChild),
    #[cfg(windows)]
    WindowsSandboxPty(SandboxedPty),
    #[cfg(windows)]
    WindowsPty(WindowsPtyChild),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ShellExitStatus {
    success: bool,
    code: Option<i32>,
}

impl RuntimeShellSessionManager {
    pub fn new(tasks: TaskRegistry) -> Self {
        Self::with_output_store(tasks, TaskOutputStore::new())
    }

    pub fn with_output_store(tasks: TaskRegistry, output_store: TaskOutputStore) -> Self {
        Self {
            tasks,
            output_store,
            sessions: HashMap::new(),
        }
    }

    pub fn output_store(&self) -> TaskOutputStore {
        self.output_store.clone()
    }

    pub fn capability_receipt(&self, id: &str) -> io::Result<CapabilityReceipt> {
        self.sessions
            .get(id)
            .map(|session| session.capability_receipt.clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("shell session '{id}' not found"),
                )
            })
    }

    pub fn spawn(&mut self, command: ShellSessionCommand) -> io::Result<ShellSessionHandle> {
        self.spawn_with_metadata_roots(command, Vec::new())
    }

    pub(crate) fn spawn_with_metadata_roots(
        &mut self,
        command: ShellSessionCommand,
        metadata_writable_directories: Vec<PathBuf>,
    ) -> io::Result<ShellSessionHandle> {
        self.spawn_with_task_registry_and_metadata_roots(
            command,
            metadata_writable_directories,
            self.tasks.clone(),
        )
    }

    pub fn spawn_with_task_registry(
        &mut self,
        command: ShellSessionCommand,
        tasks: TaskRegistry,
    ) -> io::Result<ShellSessionHandle> {
        self.spawn_with_task_registry_and_metadata_roots(command, Vec::new(), tasks)
    }

    pub(crate) fn spawn_with_task_registry_and_metadata_roots(
        &mut self,
        command: ShellSessionCommand,
        metadata_writable_directories: Vec<PathBuf>,
        tasks: TaskRegistry,
    ) -> io::Result<ShellSessionHandle> {
        let requested_terminal = command.terminal;
        let description = command.description.clone();
        let task = tasks.create_shell(description.clone(), command.command.clone());
        tasks.mark_running(&task.id).map_err(io::Error::other)?;
        let shell = ShellResolver::for_current_host()
            .resolve_from_environment()
            .map_err(io::Error::other)?;
        // An argv request is launched directly by the native adapter on
        // Windows; it does not depend on the user's configured shell
        // dialect. Shell eligibility checks must only inspect shell-script
        // requests, otherwise a PowerShell 5.1 installation would reject
        // safe commands such as `node -e ...` before the AppContainer starts.
        if command.argv.is_none() {
            ensure_shell_sandbox_supported(shell.kind(), command.sandbox)?;
        }
        let uses_seatbelt = cfg!(target_os = "macos")
            && !matches!(command.sandbox, ShellSandboxMode::DangerFullAccess);
        let capability =
            shell_effective_capability(&command, &task.id, &metadata_writable_directories)?;
        let enforcement = if matches!(command.sandbox, ShellSandboxMode::DangerFullAccess) {
            EnforcementState::Advisory
        } else if cfg!(target_os = "windows") {
            // The Windows branch below uses the native AppContainer/Job
            // adapter rather than the generic command builder.
            EnforcementState::Enforced
        } else {
            orca_tools::sandbox::enforcement_state()
        };
        let broker = ExecutionBroker::with_backend_and_ceiling(
            enforcement,
            shell_backend_name(),
            shell_capability_ceiling(&command, &metadata_writable_directories),
        );

        #[cfg(windows)]
        if !matches!(command.sandbox, ShellSandboxMode::DangerFullAccess) {
            let restricted = spawn_windows_sandbox(
                &command,
                &metadata_writable_directories,
                &shell,
                &broker,
                &capability,
            );
            let (
                child,
                process_job,
                stdin,
                stdout_reader,
                stderr_reader,
                effective_terminal,
                capability_receipt,
            ) = match restricted {
                Ok(value) => value,
                Err(error) => {
                    let _ = tasks.fail(&task.id, format!("failed to run shell: {error}"));
                    return Err(error);
                }
            };
            let output_store = self.output_store.clone();
            let reader_stop = Arc::new(AtomicBool::new(false));
            let stdout_handle = Some(spawn_output_reader(
                stdout_reader,
                output_store.clone(),
                task.id.clone(),
                ShellOutputStream::Stdout,
                Arc::clone(&reader_stop),
                false,
            ));
            let stderr_handle = stderr_reader.map(|reader| {
                spawn_output_reader(
                    reader,
                    output_store.clone(),
                    task.id.clone(),
                    ShellOutputStream::Stderr,
                    Arc::clone(&reader_stop),
                    false,
                )
            });
            if let Err(error) = tasks.mark_worker_spawned(&task.id, child.id()?) {
                cleanup_failed_shell_start(
                    child,
                    process_job,
                    stdin,
                    reader_stop,
                    stdout_handle,
                    stderr_handle,
                );
                let error = io::Error::other(error);
                let _ = tasks.fail(&task.id, format!("failed to run shell: {error}"));
                return Err(error);
            }
            let id = format!("shell-{}", Uuid::new_v4());
            self.sessions.insert(
                id.clone(),
                ShellSession {
                    tasks,
                    task_id: task.id.clone(),
                    command: command.command.clone(),
                    description,
                    child,
                    process_job,
                    stdin,
                    output_store,
                    stdout_handle,
                    stderr_handle,
                    reader_stop,
                    requested_terminal,
                    effective_terminal,
                    capability_receipt: capability_receipt.clone(),
                },
            );
            return Ok(ShellSessionHandle {
                id,
                task_id: task.id,
                requested_terminal,
                effective_terminal,
                capability_receipt,
            });
        }

        let mut process = match command.sandbox {
            ShellSandboxMode::WorkspaceWrite {
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
            } => orca_tools::sandbox::workspace_write_bash_command(
                orca_tools::sandbox::WorkspaceWriteSandboxCommandContext {
                    command: &command.command,
                    cwd: &command.cwd,
                    readable_roots: &command.additional_readable_directories,
                    additional_roots: &command.additional_working_directories,
                    metadata_writable_roots: &metadata_writable_directories,
                    denied_roots: &command.denied_working_directories,
                    network_access,
                    exclude_tmpdir_env_var,
                    exclude_slash_tmp,
                    allowed_unix_socket_roots: &command.allowed_unix_socket_roots,
                },
            ),
            ShellSandboxMode::ReadOnly {
                network_access,
                allow_global_read,
            } => orca_tools::sandbox::read_only_bash_command(
                orca_tools::sandbox::ReadOnlySandboxCommandContext {
                    command: &command.command,
                    cwd: &command.cwd,
                    readable_roots: &command.additional_readable_directories,
                    additional_roots: &command.additional_working_directories,
                    metadata_writable_roots: &metadata_writable_directories,
                    denied_roots: &command.denied_working_directories,
                    network_access,
                    allow_global_read,
                    allowed_unix_socket_roots: &command.allowed_unix_socket_roots,
                },
            ),
            ShellSandboxMode::DangerFullAccess => session_command(&shell, &command),
        };
        process.env_remove("ORCA_API_KEY");
        for (key, value) in &command.env {
            match value {
                Some(value) => {
                    process.env(key, value);
                }
                None => {
                    process.env_remove(key);
                }
            }
        }
        if uses_seatbelt {
            process.env("ORCA_SANDBOX", "seatbelt");
        }
        let stdio = configure_shell_stdio(&mut process, requested_terminal)?;
        let effective_terminal = stdio.effective_terminal();
        let initialized = spawn_configured_shell_with_broker(process, stdio, &broker, capability);
        let (child, process_job, stdin, stdout_reader, stderr_reader, capability_receipt) =
            match initialized {
                Ok(initialized) => initialized,
                Err(error) => {
                    let _ = tasks.fail(&task.id, format!("failed to run shell: {error}"));
                    return Err(error);
                }
            };
        let output_store = self.output_store.clone();
        let reader_stop = Arc::new(AtomicBool::new(false));
        let stdout_handle = Some(spawn_output_reader(
            stdout_reader,
            output_store.clone(),
            task.id.clone(),
            ShellOutputStream::Stdout,
            Arc::clone(&reader_stop),
            cfg!(unix) && effective_terminal.is_pty(),
        ));
        let stderr_handle = stderr_reader.map(|reader| {
            spawn_output_reader(
                reader,
                output_store.clone(),
                task.id.clone(),
                ShellOutputStream::Stderr,
                Arc::clone(&reader_stop),
                cfg!(unix) && effective_terminal.is_pty(),
            )
        });
        if let Err(error) = tasks.mark_worker_spawned(&task.id, child.id()?) {
            cleanup_failed_shell_start(
                child,
                process_job,
                stdin,
                reader_stop,
                stdout_handle,
                stderr_handle,
            );
            let error = io::Error::other(error);
            let _ = tasks.fail(&task.id, format!("failed to run shell: {error}"));
            return Err(error);
        }
        let id = format!("shell-{}", Uuid::new_v4());
        self.sessions.insert(
            id.clone(),
            ShellSession {
                tasks,
                task_id: task.id.clone(),
                command: command.command.clone(),
                description,
                stdin,
                child,
                process_job,
                output_store,
                stdout_handle,
                stderr_handle,
                reader_stop,
                requested_terminal,
                effective_terminal,
                capability_receipt: capability_receipt.clone(),
            },
        );

        Ok(ShellSessionHandle {
            id,
            task_id: task.id,
            requested_terminal,
            effective_terminal,
            capability_receipt,
        })
    }

    pub fn write_stdin(&mut self, id: &str, input: &str) -> io::Result<()> {
        let session = self.session_mut(id)?;
        session.stdin.write_all(id, input.as_bytes())
    }

    pub fn close_stdin(&mut self, id: &str) -> io::Result<()> {
        let session = self.session_mut(id)?;
        session.stdin.close();
        Ok(())
    }

    pub fn update_description(&mut self, id: &str, description: &str) -> io::Result<()> {
        let description = description.trim();
        if description.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shell description must not be empty",
            ));
        }
        let session = self.session_mut(id)?;
        session.description = description.to_string();
        Ok(())
    }

    pub fn resize(&mut self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        let session = self.session_mut(id)?;
        session.stdin.resize_pty(id, cols, rows)
    }

    pub fn list(&mut self) -> Vec<ShellSessionSnapshot> {
        self.sessions
            .iter_mut()
            .map(|(id, session)| {
                let status = match session.try_wait() {
                    Ok(Some(status)) if status.success() => TaskStatus::Completed,
                    Ok(Some(_)) => TaskStatus::Failed,
                    Ok(None) | Err(_) => TaskStatus::Running,
                };
                ShellSessionSnapshot {
                    id: id.clone(),
                    task_id: session.task_id.clone(),
                    command: session.command.clone(),
                    description: session.description.clone(),
                    status,
                    requested_terminal: session.requested_terminal,
                    effective_terminal: session.effective_terminal,
                }
            })
            .collect()
    }

    pub fn reap_completed(&mut self) -> io::Result<Vec<ShellSessionOutput>> {
        self.reap_completed_where(|_| true)
    }

    pub(crate) fn reap_completed_preserving_output(
        &mut self,
    ) -> io::Result<Vec<ShellSessionOutput>> {
        self.reap_completed_where_with_output_policy(|_| true, false)
    }

    pub fn reap_completed_except(
        &mut self,
        protected_ids: &HashSet<String>,
    ) -> io::Result<Vec<ShellSessionOutput>> {
        self.reap_completed_where(|id| !protected_ids.contains(id))
    }

    fn reap_completed_where(
        &mut self,
        should_reap: impl Fn(&str) -> bool,
    ) -> io::Result<Vec<ShellSessionOutput>> {
        self.reap_completed_where_with_output_policy(should_reap, true)
    }

    fn reap_completed_where_with_output_policy(
        &mut self,
        should_reap: impl Fn(&str) -> bool,
        remove_completed_output: bool,
    ) -> io::Result<Vec<ShellSessionOutput>> {
        let ids = self
            .sessions
            .iter_mut()
            .filter_map(|(id, session)| match session.try_wait() {
                Ok(Some(status)) if should_reap(id) => Some(Ok((id.clone(), status))),
                Ok(Some(_)) => None,
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut completed = Vec::new();
        for (id, status) in ids {
            completed.push(self.finish_terminal_session(&id, status, remove_completed_output)?);
        }
        Ok(completed)
    }

    pub fn reap_requested_stops(&mut self) -> io::Result<Vec<ShellSessionOutput>> {
        self.reap_requested_stops_inner(true)
    }

    pub(crate) fn reap_requested_stops_preserving_output(
        &mut self,
    ) -> io::Result<Vec<ShellSessionOutput>> {
        self.reap_requested_stops_inner(false)
    }

    fn reap_requested_stops_inner(
        &mut self,
        remove_completed_output: bool,
    ) -> io::Result<Vec<ShellSessionOutput>> {
        let ids = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                if session.tasks.is_cancelled(&session.task_id) {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let mut stopped = Vec::new();
        for id in ids {
            stopped.push(self.terminate(
                &id,
                ShellSessionTermination::Cancelled,
                remove_completed_output,
            )?);
        }
        Ok(stopped)
    }

    pub fn read(&mut self, id: &str, timeout: Duration) -> io::Result<ShellSessionOutput> {
        self.read_inner(id, timeout, true)
    }

    pub(crate) fn read_preserving_output(
        &mut self,
        id: &str,
        timeout: Duration,
    ) -> io::Result<ShellSessionOutput> {
        self.read_inner(id, timeout, false)
    }

    fn read_inner(
        &mut self,
        id: &str,
        timeout: Duration,
        remove_completed_output: bool,
    ) -> io::Result<ShellSessionOutput> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            let session = self.session_mut(id)?;
            if let Some(status) = session.try_wait()? {
                return self.finish_terminal_session(id, status, remove_completed_output);
            }
            if session.output_size() > 0 || Instant::now() >= deadline {
                return Ok(session.output(
                    id,
                    TaskStatus::Running,
                    None,
                    ShellSessionTermination::Running,
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn read_output_delta(
        &self,
        task_id: &str,
        from_offset: usize,
        max_bytes: usize,
    ) -> io::Result<TaskOutputRead> {
        self.output_store
            .read_delta(task_id, from_offset, max_bytes)
    }

    pub(crate) fn remove_output(&self, task_id: &str) -> bool {
        self.output_store.remove(task_id)
    }

    pub fn wait(&mut self, id: &str, timeout: Duration) -> io::Result<ShellSessionOutput> {
        self.wait_or_cancel(id, timeout, || false)
    }

    pub fn wait_or_cancel(
        &mut self,
        id: &str,
        timeout: Duration,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<ShellSessionOutput> {
        self.wait_or_cancel_with_output(id, timeout, should_cancel, &mut |_| {})
    }

    pub(crate) fn wait_or_cancel_with_output(
        &mut self,
        id: &str,
        timeout: Duration,
        should_cancel: impl Fn() -> bool,
        on_output: &mut dyn FnMut(&str),
    ) -> io::Result<ShellSessionOutput> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let task_id = self.session_mut(id)?.task_id.clone();
        let mut output_offset = 0;
        loop {
            let completed = self.session_mut(id)?.try_wait()?.is_some();
            output_offset = self.emit_available_output(&task_id, output_offset, on_output)?;
            if completed {
                break;
            }
            if should_cancel() {
                return self.kill(id);
            }
            if Instant::now() >= deadline {
                return self.terminate(id, ShellSessionTermination::TimedOut, true);
            }
            thread::sleep(Duration::from_millis(25));
        }

        let mut session = self.take_session(id)?;
        let status = session.finish_after_exit()?;
        self.emit_available_output(&task_id, output_offset, on_output)?;
        let tasks = session.tasks.clone();
        let output = session.output(
            id,
            if status.success() {
                TaskStatus::Completed
            } else {
                TaskStatus::Failed
            },
            process_exit_code(status),
            ShellSessionTermination::Exited,
        );
        Self::record_terminal_output(&tasks, &output)?;
        self.output_store.remove(&output.task_id);
        Ok(output)
    }

    fn emit_available_output(
        &self,
        task_id: &str,
        from_offset: usize,
        on_output: &mut dyn FnMut(&str),
    ) -> io::Result<usize> {
        let output = self
            .output_store
            .read_delta(task_id, from_offset, usize::MAX)?;
        if !output.combined.is_empty() {
            on_output(&output.combined);
        }
        Ok(output.next_offset)
    }

    pub fn kill(&mut self, id: &str) -> io::Result<ShellSessionOutput> {
        self.terminate(id, ShellSessionTermination::Cancelled, true)
    }

    pub(crate) fn kill_preserving_output(&mut self, id: &str) -> io::Result<ShellSessionOutput> {
        self.terminate(id, ShellSessionTermination::Cancelled, false)
    }

    fn terminate(
        &mut self,
        id: &str,
        termination: ShellSessionTermination,
        remove_completed_output: bool,
    ) -> io::Result<ShellSessionOutput> {
        debug_assert!(matches!(
            termination,
            ShellSessionTermination::Cancelled | ShellSessionTermination::TimedOut
        ));
        let mut session = self.take_session(id)?;
        let tasks = session.tasks.clone();
        if wait_for_process_exit(&mut session, Duration::from_millis(150))?.is_some() {
            let status = session.finish_after_exit()?;
            let output = session.output(
                id,
                if status.success() {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                },
                process_exit_code(status),
                ShellSessionTermination::Exited,
            );
            Self::record_terminal_output(&tasks, &output)?;
            if remove_completed_output {
                self.output_store.remove(&output.task_id);
            }
            return Ok(output);
        }
        session.terminate_child_tree();
        let status = session.child.wait()?;
        session.join_readers();
        let output = session.output(
            id,
            TaskStatus::Stopped,
            process_exit_code(status),
            termination,
        );
        tasks
            .stop(&output.task_id, output.stdout.clone())
            .map_err(io::Error::other)?;
        if remove_completed_output {
            self.output_store.remove(&output.task_id);
        }
        Ok(output)
    }

    pub fn terminate_all(&mut self) {
        let ids = self.sessions.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            let _ = self.terminate(&id, ShellSessionTermination::Cancelled, true);
        }
    }

    fn finish_terminal_session(
        &mut self,
        id: &str,
        _observed_status: ShellExitStatus,
        remove_completed_output: bool,
    ) -> io::Result<ShellSessionOutput> {
        let mut session = self.take_session(id)?;
        let status = session.finish_after_exit()?;
        let tasks = session.tasks.clone();
        let output = session.output(
            id,
            if status.success() {
                TaskStatus::Completed
            } else {
                TaskStatus::Failed
            },
            process_exit_code(status),
            ShellSessionTermination::Exited,
        );
        Self::record_terminal_output(&tasks, &output)?;
        if remove_completed_output {
            self.output_store.remove(&output.task_id);
        }
        Ok(output)
    }

    fn record_terminal_output(tasks: &TaskRegistry, output: &ShellSessionOutput) -> io::Result<()> {
        if output.status == TaskStatus::Completed {
            tasks
                .complete(&output.task_id, output.stdout.clone())
                .map_err(io::Error::other)
        } else {
            tasks
                .fail(&output.task_id, output.stderr_or_stdout())
                .map_err(io::Error::other)
        }
    }

    fn session_mut(&mut self, id: &str) -> io::Result<&mut ShellSession> {
        self.sessions.get_mut(id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("shell session '{id}' not found"),
            )
        })
    }

    fn take_session(&mut self, id: &str) -> io::Result<ShellSession> {
        self.sessions.remove(id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("shell session '{id}' not found"),
            )
        })
    }
}

fn ensure_shell_sandbox_supported(shell: ShellKind, sandbox: ShellSandboxMode) -> io::Result<()> {
    #[cfg(windows)]
    {
        if matches!(shell, ShellKind::PowerShell(PowerShellEdition::Windows))
            && windows_sandbox_uses_appcontainer(sandbox)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows PowerShell 5.1 runs in ConstrainedLanguage inside the AppContainer sandbox and cannot satisfy Orca's shell contract; install PowerShell 7 (pwsh.exe) or use cmd.exe",
            ));
        }
        if !matches!(sandbox, ShellSandboxMode::DangerFullAccess)
            && matches!(shell, ShellKind::GitBash)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Git Bash is not an eligible Windows sandbox shell",
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        if matches!(
            shell,
            ShellKind::PowerShell(_) | ShellKind::Cmd | ShellKind::GitBash
        ) && !matches!(sandbox, ShellSandboxMode::DangerFullAccess)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows shell sandbox is not available for the requested permission mode",
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn windows_sandbox_uses_appcontainer(sandbox: ShellSandboxMode) -> bool {
    match sandbox {
        ShellSandboxMode::WorkspaceWrite { network_access, .. } => !network_access,
        ShellSandboxMode::ReadOnly {
            network_access,
            allow_global_read,
        } => !network_access || !allow_global_read,
        ShellSandboxMode::DangerFullAccess => false,
    }
}

#[cfg(windows)]
fn spawn_windows_sandbox(
    command: &ShellSessionCommand,
    metadata_writable_directories: &[PathBuf],
    shell: &ShellSpec,
    broker: &ExecutionBroker,
    capability: &EffectiveCapability,
) -> io::Result<(
    ShellChild,
    ProcessJob,
    ShellInput,
    Box<dyn Read + Send>,
    Option<Box<dyn Read + Send>>,
    ShellTerminalMode,
    CapabilityReceipt,
)> {
    let capability_receipt = broker.authorize_platform(capability).map_err(|error| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Windows sandbox authorization rejected: {error:?}"),
        )
    })?;
    let (mode, network_access) = match command.sandbox {
        ShellSandboxMode::WorkspaceWrite { network_access, .. } => {
            (SandboxFilesystemMode::WorkspaceWrite, network_access)
        }
        ShellSandboxMode::ReadOnly {
            network_access,
            allow_global_read,
        } => (
            SandboxFilesystemMode::ReadOnly { allow_global_read },
            network_access,
        ),
        ShellSandboxMode::DangerFullAccess => unreachable!("full access uses std process spawn"),
    };
    let spec = session_command_spec(shell, command)?;
    // AppContainer is denied by default from reading runner-managed runtime
    // directories (for example the hosted Node or PowerShell install). Grant
    // only the resolved executable's parent so the child can load its binary
    // and adjacent runtime files; user data remains governed by the plan.
    let mut readable_roots = command.additional_readable_directories.clone();
    if let Some(parent) = spec.program.parent()
        && !readable_roots.iter().any(|root| root == parent)
    {
        readable_roots.push(parent.to_path_buf());
    }
    let mut writable_roots = command.additional_working_directories.clone();
    writable_roots.extend_from_slice(metadata_writable_directories);
    let plan = WindowsSandboxPlan::build(WindowsSandboxPolicyInput {
        mode,
        cwd: command.cwd.clone(),
        readable_roots,
        writable_roots,
        denied_roots: command.denied_working_directories.clone(),
        network_access,
    })
    .map_err(io::Error::other)?;
    let capability_root = orca_core::config::folder_trust::config_dir()
        .unwrap_or_else(|| command.cwd.join(".orca"))
        .join("windows-capabilities");
    let capabilities = CapabilityStore::new(capability_root);
    #[cfg(test)]
    capabilities
        .provision_setup(&command.cwd, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .map_err(io::Error::other)?;
    #[cfg(not(test))]
    capabilities
        .verify_setup_for_workspace(&command.cwd, orca_windows_sandbox::SETUP_HELPER_VERSION)
        .map_err(io::Error::other)?;
    let request = || SandboxSpawnRequest {
        program: &spec.program,
        args: &spec.args,
        cwd: &command.cwd,
        env: &command.env,
        plan: &plan,
        capabilities: &capabilities,
    };
    let terminal = resolve_terminal_support(command.terminal, native_pty_supported())?;
    if let ShellTerminalMode::Pty { cols, rows } = terminal {
        let mut child = SandboxedPty::spawn(request(), cols, rows).map_err(io::Error::other)?;
        let process_job = child.take_process_job()?;
        let (input, output) = child.take_pty()?;
        return Ok((
            ShellChild::WindowsSandboxPty(child),
            process_job,
            ShellInput::WindowsSandboxPty(input),
            output,
            None,
            terminal,
            capability_receipt,
        ));
    }

    let mut child = SandboxedChild::spawn(request()).map_err(io::Error::other)?;
    let process_job = child.take_process_job()?;
    let (stdin, stdout, stderr) = child.take_stdio()?;
    Ok((
        ShellChild::WindowsSandbox(child),
        process_job,
        ShellInput::WindowsSandbox(Some(stdin)),
        stdout,
        Some(stderr),
        terminal,
        capability_receipt,
    ))
}

fn shell_command(shell: &ShellSpec, script: &str, cwd: &std::path::Path) -> std::process::Command {
    let command = shell.command(script);
    let mut process = std::process::Command::new(command.program);
    process.args(command.args).current_dir(cwd);
    orca_tools::process::prepare_non_interactive_command(&mut process);
    process
}

fn session_command(shell: &ShellSpec, command: &ShellSessionCommand) -> std::process::Command {
    #[cfg(windows)]
    if let Some(argv) = command.argv.as_ref() {
        let mut process = direct_command(argv, &command.cwd);
        orca_tools::process::prepare_non_interactive_command(&mut process);
        return process;
    }
    shell_command(shell, &command.command, &command.cwd)
}

#[cfg(windows)]
fn session_command_spec(
    shell: &ShellSpec,
    command: &ShellSessionCommand,
) -> io::Result<orca_platform::shell::CommandSpec> {
    use std::ffi::OsString;

    let Some(argv) = command.argv.as_ref() else {
        return Ok(shell.command(&command.command));
    };
    let (program, args) = argv.split_first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "command argv must not be empty",
        )
    })?;
    let program =
        orca_platform::shell::resolve_program(program).unwrap_or_else(|| PathBuf::from(program));
    Ok(orca_platform::shell::CommandSpec {
        program,
        args: args.iter().map(OsString::from).collect(),
    })
}

#[cfg(windows)]
fn direct_command(argv: &[String], cwd: &std::path::Path) -> std::process::Command {
    let (program, args) = argv
        .split_first()
        .expect("validated command/exec argv must not be empty");
    let program =
        orca_platform::shell::resolve_program(program).unwrap_or_else(|| PathBuf::from(program));
    let mut command = std::process::Command::new(program);
    command.args(args).current_dir(cwd);
    command
}
impl Drop for RuntimeShellSessionManager {
    fn drop(&mut self) {
        self.terminate_all();
    }
}

impl ShellSession {
    fn join_readers(&mut self) {
        self.terminate_child_tree();
        let _ = self.child.wait();
        self.stop_and_join_readers();
    }

    fn stop_and_join_readers(&mut self) {
        self.stdin.close();
        self.stdin.close_terminal();
        self.reader_stop.store(true, Ordering::Release);
        if let Some(handle) = self.stdout_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_handle.take() {
            let _ = handle.join();
        }
    }

    fn finish_after_exit(&mut self) -> io::Result<ShellExitStatus> {
        if cfg!(unix) && self.effective_terminal.is_pty() {
            self.stdin.close();
            thread::sleep(Duration::from_millis(50));
            self.stop_and_join_readers();
            self.terminate_child_tree();
            self.child.wait()
        } else {
            let status = self.child.wait()?;
            self.drain_windows_terminal_output();
            self.stop_and_join_readers();
            Ok(status)
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ShellExitStatus>> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if self.effective_terminal.is_pty() {
            return child_status_without_reaping(self.child.id()?);
        }
        self.child.try_wait()
    }

    fn output(
        &self,
        id: &str,
        status: TaskStatus,
        exit_code: Option<i32>,
        termination: ShellSessionTermination,
    ) -> ShellSessionOutput {
        let output = self
            .output_store
            .read_delta(&self.task_id, 0, usize::MAX)
            .unwrap_or_else(|_| TaskOutputRead {
                stdout: String::new(),
                stderr: String::new(),
                combined: String::new(),
                next_offset: 0,
                bytes_read: 0,
                bytes_total: self.output_size(),
                omitted_prefix_bytes: 0,
                stdout_prefix_bytes: 0,
                stderr_prefix_bytes: 0,
            });
        let (stdout, stderr) = shell_output_text_with_omitted_prefix(
            output.stdout,
            output.stderr,
            output.omitted_prefix_bytes,
        );
        ShellSessionOutput {
            id: id.to_string(),
            task_id: self.task_id.clone(),
            stdout,
            stderr,
            exit_code,
            status,
            termination,
            requested_terminal: self.requested_terminal,
            effective_terminal: self.effective_terminal,
        }
    }

    fn output_size(&self) -> usize {
        self.output_store.size(&self.task_id)
    }

    fn drain_windows_terminal_output(&self) {
        #[cfg(windows)]
        if self.effective_terminal.is_pty() {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut observed = self.output_size();
            let mut quiet_since = Instant::now();
            loop {
                thread::sleep(Duration::from_millis(10));
                let current = self.output_size();
                if current != observed {
                    observed = current;
                    quiet_since = Instant::now();
                }
                if quiet_since.elapsed() >= Duration::from_millis(200) || Instant::now() >= deadline
                {
                    break;
                }
            }
        }
    }

    fn terminate_child_tree(&mut self) {
        let _ = self.process_job.terminate(137);
        #[cfg(not(windows))]
        self.child.kill();
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        self.stdin.close();
        self.join_readers();
    }
}

impl SpawnedShellChild {
    fn new(child: ShellChild) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut ShellChild {
        self.child.as_mut().expect("spawned shell child")
    }

    fn into_child(mut self) -> ShellChild {
        self.child.take().expect("spawned shell child")
    }
}

fn cleanup_failed_shell_start(
    mut child: ShellChild,
    process_job: ProcessJob,
    mut stdin: ShellInput,
    reader_stop: Arc<AtomicBool>,
    stdout_handle: Option<thread::JoinHandle<()>>,
    stderr_handle: Option<thread::JoinHandle<()>>,
) {
    let _ = process_job.terminate(137);
    child.kill();
    let _ = child.wait();
    stdin.close();
    reader_stop.store(true, Ordering::Release);
    if let Some(handle) = stdout_handle {
        let _ = handle.join();
    }
    if let Some(handle) = stderr_handle {
        let _ = handle.join();
    }
}

impl Drop for SpawnedShellChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            child.kill();
            let _ = child.wait();
        }
    }
}

impl ShellChild {
    fn id(&self) -> io::Result<u32> {
        match self {
            Self::Process(child) => Ok(child.id()),
            #[cfg(windows)]
            Self::WindowsSandbox(child) => Ok(child.id()),
            #[cfg(windows)]
            Self::WindowsSandboxPty(child) => Ok(child.id()),
            #[cfg(windows)]
            Self::WindowsPty(child) => child.id(),
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ShellExitStatus>> {
        match self {
            Self::Process(child) => child.try_wait().map(|status| status.map(Into::into)),
            #[cfg(windows)]
            Self::WindowsSandbox(child) => child.try_wait().map(|status| {
                status.map(|status| ShellExitStatus {
                    success: status.success(),
                    code: status.code(),
                })
            }),
            #[cfg(windows)]
            Self::WindowsSandboxPty(child) => child.try_wait().map(|status| {
                status.map(|status| ShellExitStatus {
                    success: status.success(),
                    code: status.code(),
                })
            }),
            #[cfg(windows)]
            Self::WindowsPty(child) => child.try_wait().map(|status| {
                status.map(|status| ShellExitStatus {
                    success: status.success(),
                    code: status.code(),
                })
            }),
        }
    }

    fn wait(&mut self) -> io::Result<ShellExitStatus> {
        match self {
            Self::Process(child) => child.wait().map(Into::into),
            #[cfg(windows)]
            Self::WindowsSandbox(child) => child.wait().map(|status| ShellExitStatus {
                success: status.success(),
                code: status.code(),
            }),
            #[cfg(windows)]
            Self::WindowsSandboxPty(child) => child.wait().map(|status| ShellExitStatus {
                success: status.success(),
                code: status.code(),
            }),
            #[cfg(windows)]
            Self::WindowsPty(child) => child.wait().map(|status| ShellExitStatus {
                success: status.success(),
                code: status.code(),
            }),
        }
    }

    fn kill(&mut self) {
        match self {
            Self::Process(child) => orca_tools::process::kill_child_tree(child),
            #[cfg(windows)]
            Self::WindowsSandbox(child) => {
                let _ = child.kill();
            }
            #[cfg(windows)]
            Self::WindowsSandboxPty(child) => {
                let _ = child.kill();
            }
            #[cfg(windows)]
            Self::WindowsPty(child) => {
                let _ = child.kill();
            }
        }
    }

    fn process_mut(&mut self) -> &mut Child {
        match self {
            Self::Process(child) => child,
            #[cfg(windows)]
            Self::WindowsSandbox(_) => unreachable!("sandbox child is not a standard child"),
            #[cfg(windows)]
            Self::WindowsSandboxPty(_) => {
                unreachable!("sandbox ConPTY child is not a standard child")
            }
            #[cfg(windows)]
            Self::WindowsPty(_) => unreachable!("ConPTY child is not a standard process child"),
        }
    }
}

impl From<std::process::ExitStatus> for ShellExitStatus {
    fn from(status: std::process::ExitStatus) -> Self {
        Self {
            success: status.success(),
            code: status.code().or_else(|| {
                #[cfg(unix)]
                {
                    status.signal().map(|signal| 128 + signal)
                }
                #[cfg(not(unix))]
                {
                    None
                }
            }),
        }
    }
}

impl ShellExitStatus {
    fn success(self) -> bool {
        self.success
    }
}

impl ShellInput {
    fn write_all(&mut self, id: &str, input: &[u8]) -> io::Result<()> {
        match self {
            Self::Pipe(Some(stdin)) => stdin.write_all(input),
            Self::Pipe(None) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("shell session '{id}' stdin is closed"),
            )),
            #[cfg(windows)]
            Self::WindowsSandbox(Some(stdin)) => stdin.write_all(input),
            #[cfg(windows)]
            Self::WindowsSandbox(None) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("shell session '{id}' stdin is closed"),
            )),
            #[cfg(windows)]
            Self::WindowsSandboxPty(pty) => pty.write_all(input),
            #[cfg(unix)]
            Self::UnixPty(master) => master.write_all(input),
            #[cfg(windows)]
            Self::WindowsPty(pty) => pty.write_all(input),
        }
    }

    fn close(&mut self) {
        match self {
            Self::Pipe(stdin) => {
                stdin.take();
            }
            #[cfg(windows)]
            Self::WindowsSandbox(stdin) => {
                stdin.take();
            }
            #[cfg(windows)]
            Self::WindowsSandboxPty(pty) => pty.close(),
            #[cfg(unix)]
            Self::UnixPty(_) => {}
            #[cfg(windows)]
            Self::WindowsPty(pty) => pty.close(),
        }
    }

    fn close_terminal(&mut self) {
        match self {
            #[cfg(windows)]
            Self::WindowsSandboxPty(pty) => pty.close_terminal(),
            #[cfg(windows)]
            Self::WindowsPty(pty) => pty.close_terminal(),
            _ => {}
        }
    }

    fn resize_pty(&mut self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        match self {
            Self::Pipe(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("shell session '{id}' is not a PTY"),
            )),
            #[cfg(windows)]
            Self::WindowsSandbox(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("shell session '{id}' is not a PTY"),
            )),
            #[cfg(windows)]
            Self::WindowsSandboxPty(pty) => pty.resize(cols, rows),
            #[cfg(unix)]
            Self::UnixPty(master) => resize_pty(master, cols, rows),
            #[cfg(windows)]
            Self::WindowsPty(pty) => pty.resize(cols, rows),
        }
    }
}

enum ShellStdio {
    Pipe,
    #[cfg(unix)]
    Pty {
        master: File,
        cols: Option<u16>,
        rows: Option<u16>,
    },
    #[cfg(windows)]
    WindowsPty {
        cols: Option<u16>,
        rows: Option<u16>,
    },
}

impl ShellStdio {
    fn effective_terminal(&self) -> ShellTerminalMode {
        match self {
            Self::Pipe => ShellTerminalMode::pipe(),
            #[cfg(unix)]
            Self::Pty { cols, rows, .. } => ShellTerminalMode::pty(*cols, *rows),
            #[cfg(windows)]
            Self::WindowsPty { cols, rows } => ShellTerminalMode::pty(*cols, *rows),
        }
    }
}

fn configure_shell_stdio(
    process: &mut std::process::Command,
    terminal: ShellTerminalMode,
) -> io::Result<ShellStdio> {
    match resolve_terminal_support(terminal, native_pty_supported())? {
        ShellTerminalMode::Pipe => {
            process
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(unix)]
            {
                process.process_group(0);
            }
            Ok(ShellStdio::Pipe)
        }
        ShellTerminalMode::Pty { cols, rows } => configure_pty_stdio(process, cols, rows),
    }
}

#[cfg(target_os = "macos")]
fn shell_backend_name() -> &'static str {
    "seatbelt"
}

#[cfg(target_os = "linux")]
fn shell_backend_name() -> &'static str {
    "bwrap+landlock+seccomp"
}

#[cfg(target_os = "windows")]
fn shell_backend_name() -> &'static str {
    "windows-sandbox"
}

#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
fn shell_backend_name() -> &'static str {
    "platform-process"
}

fn shell_effective_capability(
    command: &ShellSessionCommand,
    request_id: &str,
    metadata_writable_directories: &[PathBuf],
) -> io::Result<EffectiveCapability> {
    let (process_class, capabilities) = match command.sandbox {
        ShellSandboxMode::DangerFullAccess => (
            CapabilityProcessClass::UserTrustedIntegration,
            CapabilitySet::all(),
        ),
        ShellSandboxMode::WorkspaceWrite { network_access, .. } => (
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet {
                read: true,
                write: true,
                metadata_write: !metadata_writable_directories.is_empty(),
                network: network_access,
                shell: true,
                agent: false,
            },
        ),
        ShellSandboxMode::ReadOnly { network_access, .. } => (
            CapabilityProcessClass::SandboxedTool,
            CapabilitySet {
                read: true,
                // Read-only profiles can still carry an explicit write
                // overlay. The platform builders grant writes only to the
                // listed additional roots, never to the workspace cwd.
                write: !command.additional_working_directories.is_empty(),
                metadata_write: !metadata_writable_directories.is_empty(),
                network: network_access,
                shell: true,
                agent: false,
            },
        ),
    };
    let mut request =
        CapabilityRequest::new(request_id, process_class, capabilities, command.cwd.clone());
    request.read_roots = command.additional_readable_directories.clone();
    request.write_roots = command.additional_working_directories.clone();
    request.metadata_roots = metadata_writable_directories.to_vec();
    request.denied_roots = command.denied_working_directories.clone();
    let resolved = if process_class == CapabilityProcessClass::UserTrustedIntegration {
        EffectiveCapability::resolve_user_trusted(
            request,
            CapabilitySet::all(),
            orca_core::approval_types::ApprovalMode::FullAuto,
        )
    } else {
        EffectiveCapability::resolve(
            request,
            CapabilitySet::all(),
            orca_core::approval_types::ApprovalMode::FullAuto,
        )
    };
    resolved.map_err(|error| io::Error::other(format!("invalid shell capability: {error:?}")))
}

fn shell_capability_ceiling(
    command: &ShellSessionCommand,
    metadata_writable_directories: &[PathBuf],
) -> orca_core::capability::CapabilityCeiling {
    let capabilities = match command.sandbox {
        ShellSandboxMode::DangerFullAccess => CapabilitySet::all(),
        ShellSandboxMode::WorkspaceWrite { network_access, .. } => CapabilitySet {
            read: true,
            write: true,
            metadata_write: !metadata_writable_directories.is_empty(),
            network: network_access,
            shell: true,
            agent: false,
        },
        ShellSandboxMode::ReadOnly { network_access, .. } => CapabilitySet {
            read: true,
            write: !command.additional_working_directories.is_empty(),
            metadata_write: !metadata_writable_directories.is_empty(),
            network: network_access,
            shell: true,
            agent: false,
        },
    };
    let mut read_roots = vec![command.cwd.clone()];
    read_roots.extend(command.additional_readable_directories.iter().cloned());
    read_roots.extend(command.additional_working_directories.iter().cloned());
    read_roots.extend(metadata_writable_directories.iter().cloned());
    read_roots.extend(command.allowed_unix_socket_roots.iter().cloned());
    read_roots.sort();
    read_roots.dedup();

    let write_roots = if capabilities.write {
        let mut roots = match command.sandbox {
            ShellSandboxMode::WorkspaceWrite { .. } => vec![command.cwd.clone()],
            ShellSandboxMode::ReadOnly { .. } => Vec::new(),
            ShellSandboxMode::DangerFullAccess => Vec::new(),
        };
        roots.extend(command.additional_working_directories.iter().cloned());
        roots.sort();
        roots.dedup();
        Some(roots)
    } else {
        Some(Vec::new())
    };
    let metadata_roots = if capabilities.metadata_write {
        Some(metadata_writable_directories.to_vec())
    } else {
        Some(Vec::new())
    };
    orca_core::capability::CapabilityCeiling {
        capabilities,
        read_roots: Some(read_roots),
        write_roots,
        metadata_roots,
        denied_roots: command.denied_working_directories.clone(),
        // Network target enforcement is not implemented by the current OS
        // adapters; an empty allow-list prevents a target-bearing request from
        // being silently interpreted as unrestricted network access.
        network_targets: Some(BTreeSet::new()),
    }
}

fn spawn_configured_shell_with_broker(
    process: std::process::Command,
    stdio: ShellStdio,
    broker: &ExecutionBroker,
    capability: EffectiveCapability,
) -> io::Result<(
    ShellChild,
    ProcessJob,
    ShellInput,
    Box<dyn Read + Send>,
    Option<Box<dyn Read + Send>>,
    CapabilityReceipt,
)> {
    #[cfg(windows)]
    if matches!(&stdio, ShellStdio::WindowsPty { .. }) {
        let ShellStdio::WindowsPty { cols, rows } = stdio else {
            unreachable!("matched ConPTY stdio")
        };
        let spawned = spawn_windows_pty(&process, cols, rows)?;
        return Ok((
            ShellChild::WindowsPty(spawned.child),
            spawned.process_job,
            ShellInput::WindowsPty(spawned.input),
            spawned.reader,
            None,
            capability.receipt(broker.enforcement(), "windows-conpty"),
        ));
    }

    let launched = if capability.process_class == CapabilityProcessClass::UserTrustedIntegration {
        broker.launch_user_trusted(
            process,
            capability.request_id.clone(),
            capability.cwd.clone(),
            capability.capabilities.clone(),
        )
    } else {
        broker.launch(process, capability)
    }
    .map_err(|error| io::Error::other(format!("execution broker launch failed: {error:?}")))?;
    let receipt = launched.receipt.clone();
    let (child, process_job) = (launched.child, launched.process_job);
    let mut child = SpawnedShellChild::new(ShellChild::Process(child));
    let (stdin, stdout_reader, stderr_reader) = stdio.finish(child.child_mut())?;
    Ok((
        child.into_child(),
        process_job,
        stdin,
        stdout_reader,
        stderr_reader,
        receipt,
    ))
}

fn resolve_terminal_support(
    requested: ShellTerminalMode,
    platform_supports_pty: bool,
) -> io::Result<ShellTerminalMode> {
    match requested {
        ShellTerminalMode::Pty { .. } if !platform_supports_pty => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the requested PTY is unavailable on this host",
        )),
        other => Ok(other),
    }
}

#[cfg(unix)]
fn configure_pty_stdio(
    process: &mut std::process::Command,
    cols: Option<u16>,
    rows: Option<u16>,
) -> io::Result<ShellStdio> {
    let (master_fd, slave_fd) = open_pty(cols, rows)?;
    let master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    process
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    process.process_group(0);
    Ok(ShellStdio::Pty { master, cols, rows })
}

#[cfg(not(unix))]
fn configure_pty_stdio(
    _process: &mut std::process::Command,
    cols: Option<u16>,
    rows: Option<u16>,
) -> io::Result<ShellStdio> {
    #[cfg(windows)]
    {
        Ok(ShellStdio::WindowsPty { cols, rows })
    }
    #[cfg(not(windows))]
    {
        let _ = (cols, rows);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native PTY support is unavailable on this platform",
        ))
    }
}

#[cfg(unix)]
fn resize_pty(master: &File, cols: u16, rows: u16) -> io::Result<()> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const TIOCSWINSZ: u64 = 0x8008_7467;
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
    const TIOCSWINSZ: u64 = 0x5414;

    let winsize = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { ioctl(master.as_raw_fd(), TIOCSWINSZ, &winsize) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl ShellStdio {
    fn finish(
        self,
        child: &mut ShellChild,
    ) -> io::Result<(
        ShellInput,
        Box<dyn Read + Send>,
        Option<Box<dyn Read + Send>>,
    )> {
        match self {
            Self::Pipe => {
                let child = child.process_mut();
                let stdin = child.stdin.take();
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| io::Error::other("child process has no stdout"))?;
                let stderr = child.stderr.take();
                #[cfg(unix)]
                {
                    set_nonblocking(&stdout)?;
                    if let Some(stderr) = stderr.as_ref() {
                        set_nonblocking(stderr)?;
                    }
                }
                let stderr = stderr.map(|stderr| Box::new(stderr) as Box<dyn Read + Send>);
                Ok((ShellInput::Pipe(stdin), Box::new(stdout), stderr))
            }
            #[cfg(unix)]
            Self::Pty { master, .. } => {
                let _ = child.process_mut();
                set_nonblocking(&master)?;
                let reader = master.try_clone()?;
                Ok((ShellInput::UnixPty(master), Box::new(reader), None))
            }
            #[cfg(windows)]
            Self::WindowsPty { .. } => unreachable!("ConPTY is finalized during spawn"),
        }
    }
}

#[cfg(unix)]
fn open_pty(cols: Option<u16>, rows: Option<u16>) -> io::Result<(i32, i32)> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    unsafe extern "C" {
        fn openpty(
            amaster: *mut i32,
            aslave: *mut i32,
            name: *mut std::ffi::c_char,
            termp: *const std::ffi::c_void,
            winp: *const std::ffi::c_void,
        ) -> i32;
    }

    let mut master = -1;
    let mut slave = -1;
    let winsize = match (cols, rows) {
        (Some(cols), Some(rows)) => Some(Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        _ => None,
    };
    let winsize_ptr = winsize
        .as_ref()
        .map(|winsize| winsize as *const Winsize as *const std::ffi::c_void)
        .unwrap_or(std::ptr::null());
    let result = unsafe {
        openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            winsize_ptr,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok((master, slave))
    }
}

impl ShellSessionOutput {
    fn stderr_or_stdout(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout.trim_end(), self.stderr)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellOutputStream {
    Stdout,
    Stderr,
}

fn spawn_output_reader<R: Read + Send + 'static>(
    mut reader: R,
    output_store: TaskOutputStore,
    task_id: String,
    stream: ShellOutputStream,
    stop: Arc<AtomicBool>,
    zero_is_transient: bool,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        let mut pending = Vec::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) if zero_is_transient && !stop.load(Ordering::Acquire) => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buffer[..n]);
                    drain_valid_utf8_output(&output_store, &task_id, stream, &mut pending);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        flush_lossy_output(&output_store, &task_id, stream, &mut pending);
    })
}

#[cfg(unix)]
fn set_nonblocking(reader: &impl AsRawFd) -> io::Result<()> {
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait_for_process_exit(
    session: &mut ShellSession,
    timeout: Duration,
) -> io::Result<Option<ShellExitStatus>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        if let Some(status) = session.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn child_status_without_reaping(pid: u32) -> io::Result<Option<ShellExitStatus>> {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let status = unsafe { info.si_status() };
    let exited = info.si_code == libc::CLD_EXITED;
    Ok(Some(ShellExitStatus {
        success: exited && status == 0,
        code: exited.then_some(status),
    }))
}

fn drain_valid_utf8_output(
    output_store: &TaskOutputStore,
    task_id: &str,
    stream: ShellOutputStream,
    pending: &mut Vec<u8>,
) {
    loop {
        match str::from_utf8(pending) {
            Ok(text) => {
                append_shell_output(output_store, task_id, stream, text);
                pending.clear();
                return;
            }
            Err(error) if error.valid_up_to() > 0 => {
                let valid = error.valid_up_to();
                let text = str::from_utf8(&pending[..valid]).unwrap_or_default();
                append_shell_output(output_store, task_id, stream, text);
                pending.drain(..valid);
            }
            Err(error) if error.error_len().is_some() => {
                append_shell_output(output_store, task_id, stream, "\u{FFFD}");
                pending.drain(..error.error_len().unwrap_or(1));
            }
            Err(_) => return,
        }
    }
}

fn flush_lossy_output(
    output_store: &TaskOutputStore,
    task_id: &str,
    stream: ShellOutputStream,
    pending: &mut Vec<u8>,
) {
    if pending.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(pending).into_owned();
    append_shell_output(output_store, task_id, stream, &text);
    pending.clear();
}

fn append_shell_output(
    output_store: &TaskOutputStore,
    task_id: &str,
    stream: ShellOutputStream,
    text: &str,
) {
    let _ = match stream {
        ShellOutputStream::Stdout => output_store.append_stdout(task_id, text),
        ShellOutputStream::Stderr => output_store.append_stderr(task_id, text),
    };
}

fn shell_output_text_with_omitted_prefix(
    mut stdout: String,
    mut stderr: String,
    omitted_prefix_bytes: usize,
) -> (String, String) {
    if omitted_prefix_bytes == 0 {
        return (stdout, stderr);
    }
    let notice = format!("[{omitted_prefix_bytes} bytes of earlier output omitted]\n");
    if stdout.is_empty() {
        stderr = format!("{notice}{stderr}");
    } else {
        stdout = format!("{notice}{stdout}");
    }
    (stdout, stderr)
}

fn process_exit_code(status: ShellExitStatus) -> Option<i32> {
    status.code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_spawn_surface_requires_execution_broker() {
        let _ = spawn_configured_shell_with_broker;
    }

    #[test]
    fn read_only_profile_can_carry_only_explicit_write_roots() {
        let cwd = std::env::current_dir()
            .expect("current directory")
            .join("orca-shell-test-workspace");
        let approved = cwd.join("approved-output");
        let command = ShellSessionCommand {
            command: "true".to_string(),
            argv: None,
            cwd: cwd.clone(),
            additional_readable_directories: Vec::new(),
            additional_working_directories: vec![approved.clone()],
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: BTreeMap::new(),
            description: "explicit write root".to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox: ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: false,
            },
        };

        let effective =
            shell_effective_capability(&command, "explicit-write", &[]).expect("capability");
        let ceiling = shell_capability_ceiling(&command, &[]);

        assert!(effective.capabilities.write);
        assert_eq!(effective.write_roots, vec![approved.clone()]);
        assert_eq!(ceiling.write_roots.as_ref(), Some(&vec![approved]));
        assert!(
            !ceiling
                .write_roots
                .as_ref()
                .expect("write roots ceiling")
                .contains(&cwd)
        );
        effective
            .ensure_subset_of(&shell_capability_ceiling(&command, &[]))
            .expect("explicit root stays inside the broker ceiling");
    }
    #[cfg(windows)]
    use orca_platform::shell::{PowerShellEdition, ShellKind};

    #[test]
    fn native_pty_capabilities_match_supported_hosts() {
        let capabilities = shell_runtime_capabilities();
        assert_eq!(capabilities.supports_pty, native_pty_supported());
        assert_eq!(capabilities.supports_pty_resize, native_pty_supported());
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_shells_accept_capability_sandbox_modes() {
        for kind in [
            ShellKind::PowerShell(PowerShellEdition::Core),
            ShellKind::PowerShell(PowerShellEdition::Windows),
            ShellKind::Cmd,
        ] {
            assert!(
                ensure_shell_sandbox_supported(
                    kind,
                    ShellSandboxMode::WorkspaceWrite {
                        network_access: true,
                        exclude_tmpdir_env_var: false,
                        exclude_slash_tmp: false,
                    }
                )
                .is_ok()
            );
            assert!(
                ensure_shell_sandbox_supported(
                    kind,
                    ShellSandboxMode::ReadOnly {
                        network_access: true,
                        allow_global_read: true,
                    }
                )
                .is_ok()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_powershell_5_rejects_appcontainer_sandbox_modes() {
        let shell = ShellKind::PowerShell(PowerShellEdition::Windows);
        for sandbox in [
            ShellSandboxMode::WorkspaceWrite {
                network_access: false,
                exclude_tmpdir_env_var: false,
                exclude_slash_tmp: false,
            },
            ShellSandboxMode::ReadOnly {
                network_access: false,
                allow_global_read: false,
            },
        ] {
            let error = ensure_shell_sandbox_supported(shell, sandbox).expect_err(
                "Windows PowerShell 5.1 cannot satisfy the AppContainer shell contract",
            );
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("ConstrainedLanguage"));
        }
        assert!(
            ensure_shell_sandbox_supported(shell, ShellSandboxMode::DangerFullAccess).is_ok(),
            "an explicit Windows PowerShell 5.1 override remains valid without AppContainer"
        );
    }

    #[cfg(windows)]
    #[test]
    fn restricted_windows_pty_session_keeps_terminal_and_resizes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tasks = TaskRegistry::new("restricted-windows-conpty".to_string());
        let mut sessions = RuntimeShellSessionManager::new(tasks);
        let script = "process.stdout.write('restricted-conpty-ready'); setTimeout(() => {}, 1500);";
        let handle = sessions
            .spawn(ShellSessionCommand {
                command: script.to_string(),
                argv: Some(vec![
                    "node".to_string(),
                    "-e".to_string(),
                    script.to_string(),
                ]),
                cwd: temp.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: BTreeMap::new(),
                description: "restricted ConPTY".to_string(),
                terminal: ShellTerminalMode::pty(Some(100), Some(30)),
                sandbox: ShellSandboxMode::WorkspaceWrite {
                    network_access: true,
                    exclude_tmpdir_env_var: false,
                    exclude_slash_tmp: false,
                },
            })
            .expect("spawn restricted ConPTY session");
        assert_eq!(
            handle.effective_terminal,
            ShellTerminalMode::pty(Some(100), Some(30))
        );
        let ready_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = sessions
                .read(&handle.id, Duration::from_millis(50))
                .expect("read restricted ConPTY startup");
            if output.stdout.contains("restricted-conpty-ready") {
                break;
            }
            assert_eq!(output.status, TaskStatus::Running, "{output:?}");
            assert!(
                Instant::now() < ready_deadline,
                "restricted ConPTY did not enter the user script: {output:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        sessions
            .close_stdin(&handle.id)
            .expect("close ConPTY stdin without closing the terminal");
        sessions
            .resize(&handle.id, 120, 40)
            .expect("resize ConPTY after closing stdin");
        let output = sessions
            .wait(&handle.id, Duration::from_secs(5))
            .expect("wait for restricted ConPTY session");
        assert_eq!(output.status, TaskStatus::Completed, "{output:?}");
        assert_eq!(
            output.effective_terminal,
            ShellTerminalMode::pty(Some(100), Some(30))
        );
    }

    #[test]
    fn terminal_support_resolution_rejects_pty_without_support() {
        let error = resolve_terminal_support(ShellTerminalMode::pty(Some(120), Some(33)), false)
            .expect_err("unsupported PTY must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            resolve_terminal_support(ShellTerminalMode::pty(Some(120), Some(33)), true)
                .expect("supported PTY"),
            ShellTerminalMode::pty(Some(120), Some(33))
        );
        assert_eq!(
            resolve_terminal_support(ShellTerminalMode::pipe(), false).expect("pipe"),
            ShellTerminalMode::pipe()
        );
    }

    #[cfg(unix)]
    #[test]
    fn registration_failure_reaps_spawned_shell_process_group() {
        let temp = tempfile::tempdir().expect("tempdir");
        let started_marker = temp.path().join("started");
        let release_marker = temp.path().join("release");
        let leaked_marker = temp.path().join("leaked");
        let mut process = std::process::Command::new("sh");
        process
            .arg("-c")
            .arg(
                "printf started > \"$STARTED\"; (while [ ! -e \"$RELEASE\" ]; do sleep 0.05; done; printf leaked > \"$LEAKED\") & wait",
            )
            .env("STARTED", &started_marker)
            .env("RELEASE", &release_marker)
            .env("LEAKED", &leaked_marker)
            .current_dir(temp.path());
        let stdio = configure_shell_stdio(&mut process, ShellTerminalMode::pipe())
            .expect("configure shell stdio");
        let capability = EffectiveCapability::resolve(
            CapabilityRequest::new(
                "registration-failure",
                CapabilityProcessClass::SandboxedTool,
                CapabilitySet::workspace_write(),
                temp.path().to_path_buf(),
            ),
            CapabilitySet::all(),
            orca_core::approval_types::ApprovalMode::FullAuto,
        )
        .expect("resolve shell capability");
        let broker = ExecutionBroker::new(EnforcementState::Enforced);
        let (child, process_job, stdin, stdout_reader, stderr_reader, _receipt) =
            spawn_configured_shell_with_broker(process, stdio, &broker, capability)
                .expect("spawn configured shell");
        let output_store = TaskOutputStore::new();
        let reader_stop = Arc::new(AtomicBool::new(false));
        let stdout_handle = Some(spawn_output_reader(
            stdout_reader,
            output_store.clone(),
            "registration-failure".to_string(),
            ShellOutputStream::Stdout,
            Arc::clone(&reader_stop),
            false,
        ));
        let stderr_handle = stderr_reader.map(|reader| {
            spawn_output_reader(
                reader,
                output_store,
                "registration-failure".to_string(),
                ShellOutputStream::Stderr,
                Arc::clone(&reader_stop),
                false,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !started_marker.exists() {
            assert!(Instant::now() < deadline, "shell child did not start");
            thread::sleep(Duration::from_millis(10));
        }
        let error = io::Error::other("injected registration failure");
        cleanup_failed_shell_start(
            child,
            process_job,
            stdin,
            reader_stop,
            stdout_handle,
            stderr_handle,
        );
        assert_eq!(error.to_string(), "injected registration failure");

        std::fs::write(&release_marker, "release").expect("release descendant");
        thread::sleep(Duration::from_millis(200));
        assert!(
            !leaked_marker.exists(),
            "registration failure must reap the spawned process group"
        );
    }

    #[cfg(unix)]
    #[test]
    fn escaped_session_descendant_cannot_block_terminal_shell_read() {
        let temp = tempfile::tempdir().expect("tempdir");
        let helper = std::env::current_exe().expect("resolve test executable");
        let mut env = BTreeMap::new();
        env.insert(
            "ORCA_SHELL_ESCAPE_HELPER".to_string(),
            Some(helper.display().to_string()),
        );
        env.insert(
            "ORCA_SHELL_ESCAPE_HOLDER".to_string(),
            Some("1".to_string()),
        );
        let tasks = TaskRegistry::new("escaped-shell-session".to_string());
        let mut sessions = RuntimeShellSessionManager::new(tasks);
        let handle = sessions
            .spawn(ShellSessionCommand {
                command: "\"$ORCA_SHELL_ESCAPE_HELPER\" --exact shell_session::tests::escaped_shell_pipe_holder_helper --nocapture & printf parent-done".to_string(),
                argv: None,
                cwd: temp.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env,
                description: "escaped shell pipe holder".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn escaped shell fixture");
        let started = Instant::now();

        let output = sessions
            .wait(&handle.id, Duration::from_millis(200))
            .expect("terminal shell read should remain bounded");

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "terminal shell reader join exceeded timeout: {:?}",
            started.elapsed()
        );
        assert!(output.stdout.contains("parent-done"));
    }

    #[cfg(unix)]
    #[test]
    fn escaped_shell_pipe_holder_helper() {
        if std::env::var_os("ORCA_SHELL_ESCAPE_HOLDER").is_none() {
            return;
        }
        unsafe {
            libc::setsid();
        }
        thread::sleep(Duration::from_secs(5));
    }
}
