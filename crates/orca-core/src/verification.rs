use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};

use orca_platform::process::ProcessJob;
use orca_platform::process::read_child_pipe_interruptibly;
use orca_platform::shell::{ShellResolver, ShellSpec};

use crate::capability::CapabilitySet;
use crate::execution_broker::{ExecutionBroker, LaunchError};
use crate::retained_output::{
    DEFAULT_RETAINED_OUTPUT_BYTES, RetainedOutputSnapshot, read_to_retained,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerificationResult {
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub fn run(command: &str) -> VerificationResult {
    run_with_timeout(command, Duration::from_secs(30))
}

fn run_with_timeout(command: &str, timeout: Duration) -> VerificationResult {
    let shell = match ShellResolver::for_current_host().resolve_from_environment() {
        Ok(shell) => shell,
        Err(error) => {
            return VerificationResult {
                command: command.to_string(),
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: format!("failed to resolve verifier shell: {error}"),
            };
        }
    };
    let child_command = build_verifier_command(&shell, command);

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let broker = ExecutionBroker::with_backend(
        crate::capability::EnforcementState::Advisory,
        "verification-user-trusted",
    );
    let output = broker
        .launch_user_trusted(
            child_command,
            "verification",
            cwd,
            CapabilitySet::read_only(),
        )
        .map(|launched| (launched.child, launched.process_job))
        .and_then(|(child, process_job)| {
            wait_for_child_output_with_timeout(child, process_job, timeout)
                .map_err(LaunchError::Spawn)
        })
        .map_err(|error| match error {
            LaunchError::Cwd(error) => error,
            LaunchError::Spawn(error) => error,
            LaunchError::EnforcementUnavailable
            | LaunchError::EnforcementAdvisory
            | LaunchError::UntrustedProcessClass
            | LaunchError::CapabilityCeilingExceeded
            | LaunchError::RemoteBackendUnavailable
            | LaunchError::NetworkTargetsUnsupported => io::Error::new(
                io::ErrorKind::Unsupported,
                format!("verification broker rejected launch: {error:?}"),
            ),
        });

    match output {
        Ok(output) => VerificationResult {
            command: command.to_string(),
            success: output.status.success() && !output.timed_out,
            exit_code: if output.timed_out {
                None
            } else {
                output.status.code()
            },
            stdout: output.stdout_text().trim().to_string(),
            stderr: if output.timed_out {
                let stderr = output.stderr_text().trim().to_string();
                if stderr.is_empty() {
                    format!("verifier timed out after {}s", timeout.as_secs())
                } else {
                    format!("verifier timed out after {}s: {stderr}", timeout.as_secs())
                }
            } else {
                output.stderr_text().trim().to_string()
            },
        },
        Err(error) => VerificationResult {
            command: command.to_string(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("failed to run verifier: {error}"),
        },
    }
}

fn build_verifier_command(shell: &ShellSpec, script: &str) -> Command {
    let command_spec = shell.command(script);
    let mut command = Command::new(command_spec.program);
    command
        .args(command_spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    command
}

struct CommandOutput {
    stdout: RetainedOutputSnapshot,
    stderr: RetainedOutputSnapshot,
    status: ExitStatus,
    timed_out: bool,
}

impl CommandOutput {
    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout.rendered_bytes()).to_string()
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.rendered_bytes()).to_string()
    }
}

fn wait_for_child_output_with_timeout(
    mut child: Child,
    process_job: ProcessJob,
    timeout: Duration,
) -> io::Result<CommandOutput> {
    let child_pid = child.id();
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            return child_setup_error(&mut child, &process_job, "child process has no stdout");
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            return child_setup_error(&mut child, &process_job, "child process has no stderr");
        }
    };
    #[cfg(unix)]
    if let Err(error) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
        terminate_child_tree(&mut child, &process_job);
        let _ = child.wait();
        return Err(error);
    }
    let reader_stop = Arc::new(AtomicBool::new(false));
    let stdout_handle = spawn_stoppable_reader(stdout, Arc::clone(&reader_stop));
    let stderr_handle = spawn_stoppable_reader(stderr, Arc::clone(&reader_stop));
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(std::time::Instant::now);
    let mut timed_out = false;
    let mut status = None;
    let status = loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit_status)) => {
                    status = Some(exit_status);
                    let drain_deadline = std::time::Instant::now() + Duration::from_millis(20);
                    while (!stdout_handle.is_finished() || !stderr_handle.is_finished())
                        && std::time::Instant::now() < drain_deadline
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    if !stdout_handle.is_finished() || !stderr_handle.is_finished() {
                        timed_out = true;
                    }
                    let _ = process_job.terminate(1);
                    kill_process_group_by_pid(child_pid);
                    reader_stop.store(true, Ordering::Release);
                }
                Ok(None) => {}
                Err(error) => {
                    terminate_child_tree(&mut child, &process_job);
                    let _ = child.wait();
                    break Err(error);
                }
            }
        }
        if let Some(exit_status) = status
            && stdout_handle.is_finished()
            && stderr_handle.is_finished()
        {
            break Ok(exit_status);
        }
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            if status.is_none() {
                terminate_child_tree(&mut child, &process_job);
                status = Some(child.wait()?);
            }
            reader_stop.store(true, Ordering::Release);
            break Ok(status.expect("timed out verifier status"));
        }
        thread::sleep(Duration::from_millis(50));
    };
    reader_stop.store(true, Ordering::Release);
    let stdout = stdout_handle
        .join()
        .map_err(|_| io::Error::other("verifier stdout reader panicked"))??;
    let stderr = stderr_handle
        .join()
        .map_err(|_| io::Error::other("verifier stderr reader panicked"))??;
    Ok(CommandOutput {
        stdout,
        stderr,
        status: status?,
        timed_out,
    })
}

#[cfg(not(windows))]
fn spawn_stoppable_reader<R: Read + Send + 'static>(
    reader: R,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<RetainedOutputSnapshot>> {
    thread::spawn(move || {
        let mut reader = StoppableReader { reader, stop };
        read_to_retained(&mut reader, DEFAULT_RETAINED_OUTPUT_BYTES)
    })
}

#[cfg(windows)]
fn spawn_stoppable_reader<R: Read + std::os::windows::io::AsRawHandle + Send + 'static>(
    reader: R,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<RetainedOutputSnapshot>> {
    thread::spawn(move || {
        let mut reader = StoppableReader { reader, stop };
        read_to_retained(&mut reader, DEFAULT_RETAINED_OUTPUT_BYTES)
    })
}

struct StoppableReader<R> {
    reader: R,
    stop: Arc<AtomicBool>,
}

#[cfg(not(windows))]
impl<R: Read> Read for StoppableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        read_child_pipe_interruptibly(&mut self.reader, self.stop.as_ref(), buffer)
    }
}

#[cfg(windows)]
impl<R: Read + std::os::windows::io::AsRawHandle> Read for StoppableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        read_child_pipe_interruptibly(&mut self.reader, self.stop.as_ref(), buffer)
    }
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

fn child_setup_error<T>(
    child: &mut Child,
    process_job: &ProcessJob,
    message: &str,
) -> io::Result<T> {
    terminate_child_tree(child, process_job);
    let _ = child.wait();
    Err(io::Error::other(message))
}

fn terminate_child_tree(child: &mut Child, process_job: &ProcessJob) {
    let _ = process_job.terminate(1);
    kill_child_tree_without_job(child);
}

fn kill_child_tree_without_job(child: &mut Child) {
    kill_process_group_by_pid(child.id());
    #[cfg(not(windows))]
    let _ = child.kill();
}

fn kill_process_group_by_pid(pid: u32) {
    #[cfg(unix)]
    kill_process_group(pid);
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let pgid = -(pid as i32);
    let terminated = unsafe { kill(pgid, 15) } == 0;
    if !terminated {
        return;
    }
    thread::sleep(Duration::from_millis(50));
    unsafe {
        let _ = kill(pgid, 9);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_platform::host::{Architecture, HostPlatform, OperatingSystem};
    use orca_platform::shell::ShellResolver;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn platform_verifier_script(unix: &str, windows: &str) -> String {
        if cfg!(windows) {
            windows.to_string()
        } else {
            unix.to_string()
        }
    }

    #[test]
    fn verifier_command_uses_resolved_windows_shell_dialect() {
        let shell = ShellResolver::new(
            HostPlatform::new(OperatingSystem::Windows, Architecture::X86_64),
            |name| {
                (name == "pwsh.exe")
                    .then(|| PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"))
            },
        )
        .resolve(None)
        .expect("resolve PowerShell");

        let command = build_verifier_command(&shell, "Write-Output ok");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            command.get_program(),
            r"C:\Program Files\PowerShell\7\pwsh.exe"
        );
        assert_eq!(
            &args[..4],
            ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]
        );
        assert!(args[4].contains("[Console]::OutputEncoding"));
        assert!(args[4].ends_with("Write-Output ok"));
    }

    #[test]
    fn verifier_command_timeout_kills_descendant_processes() {
        let start = Instant::now();
        let command = platform_verifier_script(
            "printf before; sleep 10 && printf after",
            "[Console]::Out.Write('before'); [Console]::Out.Flush(); & \"$env:WINDIR\\System32\\ping.exe\" -n 11 127.0.0.1 > $null; [Console]::Out.Write('after')",
        );

        let result = run_with_timeout(&command, Duration::from_secs(2));

        assert!(
            start.elapsed() < Duration::from_secs(5),
            "verifier should not wait for descendant processes"
        );
        assert!(!result.success);
        assert!(result.stderr.contains("timed out"), "{result:?}");
        assert_eq!(result.stdout, "before");
    }

    #[test]
    fn verifier_output_is_bounded_at_ingress() {
        let command = platform_verifier_script(
            "printf HEAD; yes x | tr -d '\\n' | head -c 2097144; printf TAIL",
            "[Console]::Out.Write('HEAD'); [Console]::Out.Write([string]::new([char]'x', 2097144)); [Console]::Out.Write('TAIL')",
        );
        let result = run(&command);

        assert!(result.success, "{}", result.stderr);
        assert!(result.stdout.len() <= 1024 * 1024 + 128);
        assert!(result.stdout.starts_with("HEAD"));
        assert!(result.stdout.ends_with("TAIL"));
        assert!(result.stdout.contains("omitted"));
    }

    #[test]
    #[cfg(unix)]
    fn inherited_pipe_descendant_cannot_extend_verifier_deadline() {
        let start = Instant::now();

        let result = run_with_timeout("(sleep 5) & printf parent-done", Duration::from_millis(200));

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "verifier reader join exceeded deadline: {:?}",
            start.elapsed()
        );
        assert!(!result.success);
        assert!(result.stdout.contains("parent-done"), "{result:?}");
        assert!(result.stderr.contains("timed out"), "{result:?}");
    }

    #[test]
    #[cfg(unix)]
    fn escaped_session_descendant_cannot_extend_verifier_deadline() {
        let helper = std::env::current_exe().expect("resolve test executable");
        let command = format!(
            "ORCA_VERIFIER_ESCAPE_HOLDER=1 {helper:?} --exact verification::tests::escaped_verifier_pipe_holder_helper --nocapture & printf parent-done"
        );
        let start = Instant::now();

        let result = run_with_timeout(&command, Duration::from_millis(200));

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "escaped verifier reader join exceeded deadline: {:?}",
            start.elapsed()
        );
        assert!(!result.success);
        assert!(result.stdout.contains("parent-done"), "{result:?}");
        assert!(result.stderr.contains("timed out"), "{result:?}");
    }

    #[test]
    #[cfg(unix)]
    fn escaped_verifier_pipe_holder_helper() {
        if std::env::var_os("ORCA_VERIFIER_ESCAPE_HOLDER").is_none() {
            return;
        }
        unsafe {
            libc::setsid();
        }
        thread::sleep(Duration::from_secs(5));
    }
}
