use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use orca_core::capability::{
    CapabilityProcessClass, CapabilityReceipt, CapabilitySet, EffectiveCapability, EnforcementState,
};
use orca_core::execution_broker::{ExecutionBroker, LaunchError};
use orca_core::retained_output::{
    DEFAULT_RETAINED_OUTPUT_BYTES, RetainedOutputSnapshot, read_to_retained,
};
use orca_platform::process::ProcessJob;
use orca_platform::process::read_child_pipe_interruptibly;
use orca_platform::shell::ShellSpec;

pub const DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM: usize = DEFAULT_RETAINED_OUTPUT_BYTES;

/// All production tool subprocesses enter through the execution broker. The
/// returned receipt is deliberately available to callers even when the
/// integration is only advisory on the current host.
pub fn spawn_with_capability(
    command: Command,
    request_id: impl Into<String>,
    cwd: &Path,
    process_class: CapabilityProcessClass,
    capabilities: CapabilitySet,
    enforcement: EnforcementState,
    backend: &'static str,
) -> io::Result<(Child, ProcessJob, CapabilityReceipt)> {
    let request_id = request_id.into();
    let broker = ExecutionBroker::with_backend(enforcement, backend);
    let launched = if process_class == CapabilityProcessClass::UserTrustedIntegration {
        broker.launch_user_trusted(command, request_id, cwd.to_path_buf(), capabilities)
    } else {
        let request = orca_core::capability::CapabilityRequest::new(
            request_id,
            process_class,
            capabilities.clone(),
            cwd.to_path_buf(),
        );
        let effective = EffectiveCapability::resolve(
            request,
            capabilities,
            orca_core::approval_types::ApprovalMode::FullAuto,
        )
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("invalid process capability: {error:?}"),
            )
        })?;
        broker.launch(command, effective)
    };
    launched
        .map(|launched| (launched.child, launched.process_job, launched.receipt))
        .map_err(|error| match error {
            LaunchError::Cwd(error) => error,
            LaunchError::Spawn(error) => error,
            LaunchError::EnforcementUnavailable => io::Error::new(
                io::ErrorKind::Unsupported,
                "process sandbox enforcement is unavailable",
            ),
            LaunchError::EnforcementAdvisory => io::Error::new(
                io::ErrorKind::PermissionDenied,
                "process sandbox would be advisory for this process class",
            ),
            LaunchError::UntrustedProcessClass => io::Error::new(
                io::ErrorKind::PermissionDenied,
                "trusted process class requires the explicit user-trusted launcher",
            ),
            LaunchError::NetworkTargetsUnsupported => io::Error::new(
                io::ErrorKind::Unsupported,
                "target-scoped network capabilities are not supported by the selected backend",
            ),
            LaunchError::CapabilityCeilingExceeded => io::Error::new(
                io::ErrorKind::PermissionDenied,
                "process capability exceeds the broker hard ceiling",
            ),
        })
}

pub fn spawn_user_trusted(
    command: Command,
    request_id: impl Into<String>,
    cwd: &Path,
) -> io::Result<(Child, ProcessJob, CapabilityReceipt)> {
    spawn_with_capability(
        command,
        request_id,
        cwd,
        CapabilityProcessClass::UserTrustedIntegration,
        CapabilitySet::read_only(),
        EnforcementState::Advisory,
        "user-trusted-integration",
    )
}

pub fn shell_command(shell: &ShellSpec, script: &str) -> Command {
    let command_spec = shell.command(script);
    let mut command = Command::new(command_spec.program);
    command.args(command_spec.args);
    command
}

pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_observed_bytes: usize,
    pub stderr_observed_bytes: usize,
    pub stdout_omitted_bytes: usize,
    pub stderr_omitted_bytes: usize,
    pub status: ExitStatus,
    pub timed_out: bool,
    pub termination: CommandTermination,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandTermination {
    Exited,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Copy, Debug)]
pub struct BoundedLine<'a> {
    pub bytes: &'a [u8],
    pub observed_bytes: usize,
    pub omitted_bytes: usize,
}

#[derive(Debug)]
pub struct LineCommandOutput<T> {
    pub value: T,
    pub stdout_observed_bytes: usize,
    pub stdout_omitted_bytes: usize,
    pub oversized_lines: usize,
    pub stderr: Vec<u8>,
    pub stderr_observed_bytes: usize,
    pub stderr_omitted_bytes: usize,
    pub status: ExitStatus,
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn stdout_text(&self) -> String {
        render_retained_text(&self.stdout, self.stdout_omitted_bytes)
    }

    pub fn stderr_text(&self) -> String {
        render_retained_text(&self.stderr, self.stderr_omitted_bytes)
    }

    pub fn output_was_omitted(&self) -> bool {
        self.stdout_omitted_bytes > 0 || self.stderr_omitted_bytes > 0
    }
}

impl<T> LineCommandOutput<T> {
    pub fn stderr_text(&self) -> String {
        render_retained_text(&self.stderr, self.stderr_omitted_bytes)
    }

    pub fn output_was_omitted(&self) -> bool {
        self.stdout_omitted_bytes > 0 || self.stderr_omitted_bytes > 0
    }
}

fn render_retained_text(bytes: &[u8], omitted_bytes: usize) -> String {
    if omitted_bytes == 0 {
        return String::from_utf8_lossy(bytes).to_string();
    }
    let split = bytes.len().div_ceil(2);
    format!(
        "{}\n[{omitted_bytes} bytes of output omitted]\n{}",
        String::from_utf8_lossy(&bytes[..split]),
        String::from_utf8_lossy(&bytes[split..])
    )
}

pub fn preserve_ingress_omission_notice(output: String, omitted_bytes: usize) -> String {
    if omitted_bytes == 0 || output.contains("bytes of output omitted") {
        return output;
    }
    let compacted = "\n[... tool output micro-compacted ...]\n";
    let notice = format!(
        "\n[{omitted_bytes} bytes of output omitted at ingress; retained output micro-compacted]\n"
    );
    if output.contains(compacted) {
        output.replacen(compacted, &notice, 1)
    } else {
        format!("{output}{notice}")
    }
}

pub fn wait_for_child_output_with_timeout(
    child: Child,
    process_job: ProcessJob,
    timeout: Duration,
) -> io::Result<CommandOutput> {
    wait_for_child_output_with_timeout_or_cancel(child, process_job, timeout, || false)
}

pub fn wait_for_child_output_with_timeout_or_cancel(
    child: Child,
    process_job: ProcessJob,
    timeout: Duration,
    should_cancel: impl Fn() -> bool,
) -> io::Result<CommandOutput> {
    wait_for_child_output_with_timeout_or_cancel_and_limit(
        child,
        process_job,
        timeout,
        should_cancel,
        DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM,
    )
}

pub fn wait_for_child_output_with_timeout_or_cancel_and_limit(
    mut child: Child,
    process_job: ProcessJob,
    timeout: Duration,
    should_cancel: impl Fn() -> bool,
    max_retained_bytes_per_stream: usize,
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
    let stdout_handle = spawn_stoppable_reader(
        stdout,
        max_retained_bytes_per_stream,
        Arc::clone(&reader_stop),
    );
    let stderr_handle = spawn_stoppable_reader(
        stderr,
        max_retained_bytes_per_stream,
        Arc::clone(&reader_stop),
    );

    let status = wait_for_child_and_readers(
        &mut child,
        &process_job,
        child_pid,
        timeout,
        should_cancel,
        || stdout_handle.is_finished() && stderr_handle.is_finished(),
        reader_stop.as_ref(),
    );

    reader_stop.store(true, Ordering::Release);
    let stdout = join_reader(stdout_handle, "stdout");
    let stderr = join_reader(stderr_handle, "stderr");
    let (status, termination) = status?;
    let stdout = stdout?;
    let stderr = stderr?;

    Ok(CommandOutput {
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_observed_bytes: stdout.observed_bytes,
        stderr_observed_bytes: stderr.observed_bytes,
        stdout_omitted_bytes: stdout.omitted_bytes,
        stderr_omitted_bytes: stderr.omitted_bytes,
        status,
        timed_out: termination == CommandTermination::TimedOut,
        termination,
    })
}

pub fn wait_for_child_stdout_lines_with_timeout<T, F>(
    child: Child,
    process_job: ProcessJob,
    timeout: Duration,
    max_line_bytes: usize,
    initial: T,
    on_line: F,
) -> io::Result<LineCommandOutput<T>>
where
    T: Send + 'static,
    F: FnMut(&mut T, BoundedLine<'_>) -> io::Result<()> + Send + 'static,
{
    wait_for_child_stdout_lines_with_timeout_or_cancel(
        child,
        process_job,
        timeout,
        max_line_bytes,
        DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM,
        initial,
        || false,
        on_line,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn wait_for_child_stdout_lines_with_timeout_or_cancel<T, F>(
    mut child: Child,
    process_job: ProcessJob,
    timeout: Duration,
    max_line_bytes: usize,
    max_retained_stderr_bytes: usize,
    initial: T,
    should_cancel: impl Fn() -> bool,
    on_line: F,
) -> io::Result<LineCommandOutput<T>>
where
    T: Send + 'static,
    F: FnMut(&mut T, BoundedLine<'_>) -> io::Result<()> + Send + 'static,
{
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
    let stdout_handle = spawn_bounded_line_reader(
        stdout,
        max_line_bytes,
        initial,
        on_line,
        Arc::clone(&reader_stop),
    );
    let stderr_handle =
        spawn_stoppable_reader(stderr, max_retained_stderr_bytes, Arc::clone(&reader_stop));
    let status = wait_for_child_and_readers(
        &mut child,
        &process_job,
        child_pid,
        timeout,
        should_cancel,
        || stdout_handle.is_finished() && stderr_handle.is_finished(),
        reader_stop.as_ref(),
    );

    reader_stop.store(true, Ordering::Release);
    let stdout = stdout_handle
        .join()
        .map_err(|_| io::Error::other("stdout line reader thread panicked"));
    let stderr = join_reader(stderr_handle, "stderr");
    let (status, termination) = status?;
    let stdout = stdout??;
    let stderr = stderr?;

    Ok(LineCommandOutput {
        value: stdout.value,
        stdout_observed_bytes: stdout.observed_bytes,
        stdout_omitted_bytes: stdout.omitted_bytes,
        oversized_lines: stdout.oversized_lines,
        stderr: stderr.bytes,
        stderr_observed_bytes: stderr.observed_bytes,
        stderr_omitted_bytes: stderr.omitted_bytes,
        status,
        timed_out: termination == CommandTermination::TimedOut,
    })
}

fn wait_for_child_and_readers(
    child: &mut Child,
    process_job: &ProcessJob,
    child_pid: u32,
    timeout: Duration,
    should_cancel: impl Fn() -> bool,
    readers_finished: impl Fn() -> bool,
    reader_stop: &AtomicBool,
) -> io::Result<(ExitStatus, CommandTermination)> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut status = None;

    loop {
        if status.is_none() {
            status = try_observe_child_exit(child, process_job, child_pid, reader_stop)?;
        }
        if let Some(exit_status) = status
            && readers_finished()
        {
            return Ok((exit_status, CommandTermination::Exited));
        }
        if should_cancel() {
            if status.is_none() {
                status = try_observe_child_exit(child, process_job, child_pid, reader_stop)?;
            }
            let termination = if status.is_some() {
                CommandTermination::Exited
            } else {
                CommandTermination::Cancelled
            };
            if status.is_none() {
                terminate_child_tree(child, process_job);
                status = Some(child.wait()?);
            }
            reader_stop.store(true, Ordering::Release);
            return Ok((status.expect("cancelled child status"), termination));
        }
        if Instant::now() >= deadline {
            if status.is_none() {
                status = try_observe_child_exit(child, process_job, child_pid, reader_stop)?;
            }
            let termination = if status.is_some() {
                CommandTermination::Exited
            } else {
                CommandTermination::TimedOut
            };
            if status.is_none() {
                terminate_child_tree(child, process_job);
                status = Some(child.wait()?);
            }
            reader_stop.store(true, Ordering::Release);
            return Ok((status.expect("timed out child status"), termination));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn try_observe_child_exit(
    child: &mut Child,
    process_job: &ProcessJob,
    child_pid: u32,
    reader_stop: &AtomicBool,
) -> io::Result<Option<ExitStatus>> {
    match child.try_wait() {
        Ok(Some(exit_status)) => {
            // Retire the process-group lease while the PID still identifies
            // this operation, then release readers held by escaped descendants.
            let _ = process_job.terminate(1);
            kill_process_group_by_pid(child_pid);
            reader_stop.store(true, Ordering::Release);
            Ok(Some(exit_status))
        }
        Ok(None) => Ok(None),
        Err(error) => {
            terminate_child_tree(child, process_job);
            let _ = child.wait();
            reader_stop.store(true, Ordering::Release);
            Err(error)
        }
    }
}

struct BoundedLineRead<T> {
    value: T,
    observed_bytes: usize,
    omitted_bytes: usize,
    oversized_lines: usize,
}

fn spawn_bounded_line_reader<R, T, F>(
    reader: R,
    max_line_bytes: usize,
    mut value: T,
    mut on_line: F,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<BoundedLineRead<T>>>
where
    R: ChildPipeReader + 'static,
    T: Send + 'static,
    F: FnMut(&mut T, BoundedLine<'_>) -> io::Result<()> + Send + 'static,
{
    thread::spawn(move || {
        let mut reader = StoppableReader { reader, stop };
        let mut buffer = [0_u8; orca_core::retained_output::RETAINED_OUTPUT_READ_CHUNK_BYTES];
        let mut line = Vec::with_capacity(max_line_bytes.min(buffer.len()));
        let mut line_observed_bytes = 0usize;
        let mut observed_bytes = 0usize;
        let mut omitted_bytes = 0usize;
        let mut oversized_lines = 0usize;
        let mut first_handler_error = None;

        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            observed_bytes = observed_bytes.saturating_add(read);
            for &byte in &buffer[..read] {
                if byte == b'\n' {
                    finish_bounded_line(
                        &mut value,
                        &mut on_line,
                        &mut line,
                        line_observed_bytes,
                        &mut omitted_bytes,
                        &mut oversized_lines,
                        &mut first_handler_error,
                    );
                    line_observed_bytes = 0;
                    continue;
                }
                line_observed_bytes = line_observed_bytes.saturating_add(1);
                if line.len() < max_line_bytes {
                    line.push(byte);
                }
            }
        }

        if line_observed_bytes > 0 {
            finish_bounded_line(
                &mut value,
                &mut on_line,
                &mut line,
                line_observed_bytes,
                &mut omitted_bytes,
                &mut oversized_lines,
                &mut first_handler_error,
            );
        }

        if let Some(error) = first_handler_error {
            return Err(error);
        }
        Ok(BoundedLineRead {
            value,
            observed_bytes,
            omitted_bytes,
            oversized_lines,
        })
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_bounded_line<T, F>(
    value: &mut T,
    on_line: &mut F,
    line: &mut Vec<u8>,
    line_observed_bytes: usize,
    omitted_bytes: &mut usize,
    oversized_lines: &mut usize,
    first_handler_error: &mut Option<io::Error>,
) where
    F: FnMut(&mut T, BoundedLine<'_>) -> io::Result<()>,
{
    let retained_bytes = line.len();
    let line_omitted_bytes = line_observed_bytes.saturating_sub(retained_bytes);
    *omitted_bytes = omitted_bytes.saturating_add(line_omitted_bytes);
    if line_omitted_bytes > 0 {
        *oversized_lines = oversized_lines.saturating_add(1);
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if first_handler_error.is_none()
        && let Err(error) = on_line(
            value,
            BoundedLine {
                bytes: line,
                observed_bytes: line_observed_bytes,
                omitted_bytes: line_omitted_bytes,
            },
        )
    {
        *first_handler_error = Some(error);
    }
    line.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_platform::host::{Architecture, HostPlatform, OperatingSystem};
    use orca_platform::shell::ShellResolver;
    use std::path::PathBuf;

    #[test]
    fn shell_command_uses_the_resolved_windows_dialect() {
        let shell = ShellResolver::new(
            HostPlatform::new(OperatingSystem::Windows, Architecture::Aarch64),
            |name| (name == "cmd.exe").then(|| PathBuf::from(r"C:\Windows\System32\cmd.exe")),
        )
        .resolve(None)
        .expect("resolve cmd.exe");

        let command = shell_command(&shell, "echo ready");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(command.get_program(), r"C:\Windows\System32\cmd.exe");
        assert_eq!(args, ["/D", "/S", "/C", "echo ready"]);
    }

    #[test]
    fn child_output_is_bounded_at_ingress_with_exact_omission_counts() {
        let logical_bytes = 256 * 1024;
        let retained_bytes = 4096;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
                "printf HEAD; yes x | tr -d '\\n' | head -c {}; printf TAIL",
                logical_bytes - 8
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        prepare_non_interactive_command(&mut command);
        let (child, process_job) = spawn_test(&mut command).expect("spawn noisy child");

        let output = wait_for_child_output_with_timeout_or_cancel_and_limit(
            child,
            process_job,
            Duration::from_secs(5),
            || false,
            retained_bytes,
        )
        .expect("collect bounded output");

        assert!(output.status.success());
        assert_eq!(output.stdout_observed_bytes, logical_bytes);
        assert_eq!(output.stdout.len(), retained_bytes);
        assert_eq!(output.stdout_omitted_bytes, logical_bytes - retained_bytes);
        assert!(output.stdout.starts_with(b"HEAD"));
        assert!(output.stdout.ends_with(b"TAIL"));
        assert_eq!(output.stderr_observed_bytes, 0);
        assert_eq!(output.stderr_omitted_bytes, 0);
    }

    #[test]
    fn bounded_line_reader_caps_newline_free_stdout() {
        let logical_bytes = 256 * 1024;
        let retained_bytes = 4096;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("yes x | tr -d '\\n' | head -c {logical_bytes}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        prepare_non_interactive_command(&mut command);
        let (child, process_job) = spawn_test(&mut command).expect("spawn noisy child");

        let output = wait_for_child_stdout_lines_with_timeout(
            child,
            process_job,
            Duration::from_secs(5),
            retained_bytes,
            Vec::new(),
            |lines, line| {
                lines.push((line.bytes.to_vec(), line.observed_bytes, line.omitted_bytes));
                Ok(())
            },
        )
        .expect("collect bounded line");

        assert!(output.status.success());
        assert_eq!(output.stdout_observed_bytes, logical_bytes);
        assert_eq!(output.stdout_omitted_bytes, logical_bytes - retained_bytes);
        assert_eq!(output.oversized_lines, 1);
        assert_eq!(output.value.len(), 1);
        assert_eq!(output.value[0].0.len(), retained_bytes);
        assert_eq!(output.value[0].1, logical_bytes);
        assert_eq!(output.value[0].2, logical_bytes - retained_bytes);
    }

    #[test]
    #[cfg(unix)]
    fn bounded_line_collector_error_still_drains_and_reaps() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("(sleep 5) & printf 'first\\nsecond\\n'")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        prepare_non_interactive_command(&mut command);
        let (child, process_job) = spawn_test(&mut command).expect("spawn child");
        let started = Instant::now();

        let error = wait_for_child_stdout_lines_with_timeout(
            child,
            process_job,
            Duration::from_millis(200),
            1024,
            (),
            |_, _| Err(io::Error::other("collector rejected line")),
        )
        .expect_err("collector error should be returned");

        assert!(error.to_string().contains("collector rejected line"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "collector failure exceeded process deadline: {:?}",
            started.elapsed()
        );
    }

    #[test]
    #[cfg(unix)]
    fn inherited_pipe_descendant_cannot_extend_wait_past_deadline() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("(sleep 5) & printf parent-done")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        prepare_non_interactive_command(&mut command);
        let (child, process_job) =
            spawn_test(&mut command).expect("spawn shell with pipe descendant");
        let start = Instant::now();

        let output = wait_for_child_output_with_timeout_or_cancel_and_limit(
            child,
            process_job,
            Duration::from_millis(200),
            || false,
            1024,
        )
        .expect("bounded wait");

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "reader join exceeded process deadline: {:?}",
            start.elapsed()
        );
        assert_eq!(output.stdout_text(), "parent-done");
    }

    #[test]
    #[cfg(unix)]
    fn escaped_session_descendant_cannot_extend_wait_past_deadline() {
        let helper = std::env::current_exe().expect("resolve test executable");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(
                "\"$ORCA_PROCESS_ESCAPE_HELPER\" --exact process::tests::escaped_pipe_holder_helper --nocapture & printf parent-done",
            )
            .env("ORCA_PROCESS_ESCAPE_HELPER", helper)
            .env("ORCA_PROCESS_ESCAPE_HOLDER", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        prepare_non_interactive_command(&mut command);
        let (child, process_job) =
            spawn_test(&mut command).expect("spawn shell with escaped pipe descendant");
        let start = Instant::now();

        let output = wait_for_child_output_with_timeout_or_cancel_and_limit(
            child,
            process_job,
            Duration::from_millis(200),
            || false,
            1024,
        )
        .expect("bounded escaped-session wait");

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "escaped reader join exceeded process deadline: {:?}",
            start.elapsed()
        );
        assert!(output.stdout_text().contains("parent-done"));
    }

    #[test]
    #[cfg(unix)]
    fn escaped_pipe_holder_helper() {
        if std::env::var_os("ORCA_PROCESS_ESCAPE_HOLDER").is_none() {
            return;
        }
        unsafe {
            libc::setsid();
        }
        thread::sleep(Duration::from_secs(5));
    }

    #[test]
    fn reader_io_failure_is_returned() {
        struct FailingReader;

        impl Read for FailingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("reader failed"))
            }
        }

        let error = join_reader(spawn_reader(FailingReader, 16), "stdout").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("reader failed"));
    }

    #[test]
    fn reader_panic_is_returned() {
        let handle =
            thread::spawn(|| -> io::Result<RetainedOutputSnapshot> { panic!("reader panicked") });

        let error = join_reader(handle, "stderr").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("stderr reader thread panicked"));
    }
}

pub fn prepare_non_interactive_command(command: &mut Command) {
    command.stdin(Stdio::null());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

#[cfg(test)]
fn spawn_reader<R: Read + Send + 'static>(
    reader: R,
    max_retained_bytes: usize,
) -> thread::JoinHandle<io::Result<RetainedOutputSnapshot>> {
    thread::spawn(move || read_to_retained(reader, max_retained_bytes))
}

fn spawn_stoppable_reader<R: ChildPipeReader + 'static>(
    reader: R,
    max_retained_bytes: usize,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<io::Result<RetainedOutputSnapshot>> {
    thread::spawn(move || {
        let mut reader = StoppableReader { reader, stop };
        read_to_retained(&mut reader, max_retained_bytes)
    })
}

struct StoppableReader<R> {
    reader: R,
    stop: Arc<AtomicBool>,
}

#[cfg(not(windows))]
trait ChildPipeReader: Read + Send {
    fn read_interruptibly(&mut self, stop: &AtomicBool, buffer: &mut [u8]) -> io::Result<usize>;
}

#[cfg(not(windows))]
impl<R: Read + Send> ChildPipeReader for R {
    fn read_interruptibly(&mut self, stop: &AtomicBool, buffer: &mut [u8]) -> io::Result<usize> {
        read_child_pipe_interruptibly(self, stop, buffer)
    }
}

#[cfg(windows)]
trait ChildPipeReader: Read + std::os::windows::io::AsRawHandle + Send {
    fn read_interruptibly(&mut self, stop: &AtomicBool, buffer: &mut [u8]) -> io::Result<usize>;
}

#[cfg(windows)]
impl<R: Read + std::os::windows::io::AsRawHandle + Send> ChildPipeReader for R {
    fn read_interruptibly(&mut self, stop: &AtomicBool, buffer: &mut [u8]) -> io::Result<usize> {
        read_child_pipe_interruptibly(self, stop, buffer)
    }
}

impl<R: ChildPipeReader> Read for StoppableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reader.read_interruptibly(self.stop.as_ref(), buffer)
    }
}

#[cfg(unix)]
pub(crate) fn set_nonblocking(reader: &impl AsRawFd) -> io::Result<()> {
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

fn join_reader(
    handle: thread::JoinHandle<io::Result<RetainedOutputSnapshot>>,
    stream: &str,
) -> io::Result<RetainedOutputSnapshot> {
    handle
        .join()
        .map_err(|_| io::Error::other(format!("{stream} reader thread panicked")))?
}

#[cfg(test)]
#[allow(deprecated)]
fn spawn_test(command: &mut Command) -> io::Result<(Child, ProcessJob)> {
    ProcessJob::spawn(command)
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

pub fn terminate_child_tree(child: &mut Child, process_job: &ProcessJob) {
    let _ = process_job.terminate(1);
    kill_process_group_by_pid(child.id());
    let _ = child.kill();
}

pub fn kill_child_tree(child: &mut Child) {
    kill_process_group_by_pid(child.id());
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

    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;
    let pgid = -(pid as i32);
    let terminated = unsafe { kill(pgid, SIGTERM) } == 0;
    if !terminated {
        return;
    }
    thread::sleep(Duration::from_millis(50));
    unsafe {
        let _ = kill(pgid, SIGKILL);
    }
}

#[cfg(test)]
mod cancellation_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[cfg(unix)]
    #[test]
    fn completed_process_wins_cancellation_observed_during_callback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let release = temp.path().join("release");
        let completed = temp.path().join("completed");
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "while [ ! -e {release:?} ]; do sleep 0.01; done; printf completed; : > {completed:?}"
        ));
        prepare_non_interactive_command(&mut command);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let (child, process_job) = spawn_test(&mut command).expect("spawn child");
        let cancellation_observed = AtomicBool::new(false);

        let output = wait_for_child_output_with_timeout_or_cancel(
            child,
            process_job,
            Duration::from_secs(5),
            || {
                if !cancellation_observed.swap(true, Ordering::SeqCst) {
                    std::fs::write(&release, []).expect("release child");
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !completed.exists() && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(5));
                    }
                    assert!(completed.exists(), "child did not complete during callback");
                    thread::sleep(Duration::from_millis(50));
                }
                true
            },
        )
        .expect("wait for child");

        assert!(cancellation_observed.load(Ordering::SeqCst));
        assert!(output.status.success());
        assert_eq!(output.termination, CommandTermination::Exited);
        assert_eq!(output.stdout, b"completed");
    }
}
