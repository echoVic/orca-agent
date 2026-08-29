use orca_core::retained_output::RetainedOutput;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use orca_core::tool_types::{
    ToolOutputTruncation, ToolRequest, ToolResult, truncate_output_with_policy,
};
use orca_platform::shell::{ShellKind, ShellSpec};

use crate::process;
use crate::sandbox;

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    execute_with_policy(
        request,
        cwd,
        ToolOutputTruncation::bytes(max_bytes),
        Duration::from_secs(120),
    )
}

pub fn execute_with_policy(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
) -> ToolResult {
    execute_with_policy_or_cancel(request, cwd, output_truncation, shell_timeout, || false)
}

pub fn execute_with_policy_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_with_policy_roots_or_cancel(
        request,
        cwd,
        &[],
        output_truncation,
        shell_timeout,
        should_cancel,
    )
}

pub fn execute_with_policy_roots_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[std::path::PathBuf],
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let shell =
        match orca_platform::shell::ShellResolver::for_current_host().resolve_from_environment() {
            Ok(shell) => shell,
            Err(error) => {
                return ToolResult::failed(
                    request,
                    format!("failed to resolve host shell: {error}"),
                    None,
                );
            }
        };
    execute_with_shell_spec_roots_or_cancel(
        &shell,
        request,
        cwd,
        additional_roots,
        output_truncation,
        shell_timeout,
        should_cancel,
    )
}

pub fn execute_with_shell_spec_roots_or_cancel(
    shell: &ShellSpec,
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[std::path::PathBuf],
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "shell command is required", None);
    };

    if matches!(
        shell.kind(),
        ShellKind::PowerShell(_) | ShellKind::Cmd | ShellKind::GitBash
    ) {
        return ToolResult::failed(
            request,
            "native Windows shell execution requires the runtime permission and sandbox path",
            None,
        );
    }

    let process_command = match shell.kind() {
        ShellKind::Posix | ShellKind::GitBash => {
            sandbox::bash_command_with_additional_roots(command, cwd, additional_roots)
        }
        ShellKind::PowerShell(_) | ShellKind::Cmd => unreachable!("guarded above"),
    };
    execute_command_with_policy_or_cancel(
        request,
        process_command,
        cwd,
        output_truncation,
        shell_timeout,
        should_cancel,
    )
}

fn execute_command_with_policy_or_cancel(
    request: &ToolRequest,
    mut process_command: std::process::Command,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    process_command
        .env_remove("ORCA_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (child, process_job, _receipt) = match process::spawn_with_capability(
        process_command,
        format!("tool:bash:{}", request.id),
        cwd,
        orca_core::capability::CapabilityProcessClass::SandboxedTool,
        orca_core::capability::CapabilitySet::workspace_write(),
        sandbox::enforcement_state(),
        "tool-sandbox",
    ) {
        Ok(spawned) => spawned,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to run shell command: {error}"),
                None,
            );
        }
    };

    let output = match process::wait_for_child_output_with_timeout_or_cancel(
        child,
        process_job,
        shell_timeout,
        &should_cancel,
    ) {
        Ok(output) => output,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to wait for shell command: {error}"),
                None,
            );
        }
    };

    let ingress_truncated = output.output_was_omitted();
    let stdout = output.stdout_text().trim_end().to_string();
    let stderr = output.stderr_text().trim_end().to_string();
    if output.termination == process::CommandTermination::Cancelled {
        let message = cancelled_message(&stdout, &stderr);
        let (message, truncated) = truncate_output_with_policy(message, output_truncation);
        let message = process::preserve_ingress_omission_notice(
            message,
            output
                .stdout_omitted_bytes
                .saturating_add(output.stderr_omitted_bytes),
        );
        let mut result = ToolResult::cancelled(request, message, output.status.code());
        result.set_truncated(ingress_truncated || truncated);
        return result;
    }
    let timed_out = output.timed_out;
    if output.status.success() && !timed_out {
        let (stdout, truncated) = truncate_output_with_policy(stdout, output_truncation);
        let stdout = process::preserve_ingress_omission_notice(stdout, output.stdout_omitted_bytes);
        return ToolResult::completed(request, stdout, ingress_truncated || truncated);
    }

    let message = if timed_out {
        if stderr.is_empty() && stdout.is_empty() {
            format!("shell command timed out after {}s", shell_timeout.as_secs())
        } else if stderr.is_empty() {
            format!(
                "shell command timed out after {}s: {stdout}",
                shell_timeout.as_secs()
            )
        } else if stdout.is_empty() {
            format!(
                "shell command timed out after {}s: {stderr}",
                shell_timeout.as_secs()
            )
        } else {
            format!(
                "shell command timed out after {}s: {stdout}\n{stderr}",
                shell_timeout.as_secs()
            )
        }
    } else if stderr.is_empty() {
        stdout
    } else if stdout.is_empty() {
        stderr
    } else {
        format!("{stdout}\n{stderr}")
    };
    let (message, truncated) = truncate_output_with_policy(message, output_truncation);
    let message = process::preserve_ingress_omission_notice(
        message,
        output
            .stdout_omitted_bytes
            .saturating_add(output.stderr_omitted_bytes),
    );
    let mut result = ToolResult::failed(
        request,
        message,
        if timed_out {
            None
        } else {
            output.status.code()
        },
    );
    result.set_truncated(ingress_truncated || truncated);
    result
}

enum StreamEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

const STREAM_OUTPUT_CHANNEL_CAPACITY: usize = 8;
const STREAM_OUTPUT_READ_CHUNK_BYTES: usize = 8 * 1024;
const STREAM_LIVE_PREVIEW_BYTES: usize = orca_core::tool_types::MAX_TOOL_OUTPUT_BYTES;

pub fn execute_streaming(
    request: &ToolRequest,
    cwd: &Path,
    max_bytes: usize,
    on_output: &mut dyn FnMut(&str),
) -> ToolResult {
    execute_streaming_with_policy(
        request,
        cwd,
        ToolOutputTruncation::bytes(max_bytes),
        Duration::from_secs(120),
        on_output,
    )
}

pub fn execute_streaming_with_policy(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    on_output: &mut dyn FnMut(&str),
) -> ToolResult {
    execute_streaming_with_policy_or_cancel(
        request,
        cwd,
        output_truncation,
        shell_timeout,
        on_output,
        || false,
    )
}

pub fn execute_streaming_with_policy_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    on_output: &mut dyn FnMut(&str),
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_streaming_with_policy_roots_or_cancel(
        request,
        cwd,
        &[],
        output_truncation,
        shell_timeout,
        on_output,
        should_cancel,
    )
}

pub fn execute_streaming_with_policy_roots_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[std::path::PathBuf],
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    on_output: &mut dyn FnMut(&str),
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };
    execute_streaming_command_or_cancel(
        request,
        sandbox::bash_command_with_additional_roots(command, cwd, additional_roots),
        cwd,
        output_truncation,
        shell_timeout,
        on_output,
        should_cancel,
    )
}

/// Stream a prebuilt (typically sandboxed) shell command. Callers that derive
/// their own sandbox profile (e.g. from a permission profile) build the
/// `Command` via `sandbox::*` and pass it here.
pub fn execute_streaming_command_or_cancel(
    request: &ToolRequest,
    mut command: std::process::Command,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
    shell_timeout: Duration,
    on_output: &mut dyn FnMut(&str),
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    command
        .env_remove("ORCA_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, process_job, _receipt) = match process::spawn_with_capability(
        command,
        format!("tool:bash-stream:{}", request.id),
        cwd,
        orca_core::capability::CapabilityProcessClass::SandboxedTool,
        orca_core::capability::CapabilitySet::workspace_write(),
        sandbox::enforcement_state(),
        "tool-sandbox",
    ) {
        Ok(spawned) => spawned,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to run shell command: {error}"),
                None,
            );
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    #[cfg(unix)]
    if let Err(error) = stdout
        .as_ref()
        .map_or(Ok(()), process::set_nonblocking)
        .and_then(|()| stderr.as_ref().map_or(Ok(()), process::set_nonblocking))
    {
        process::terminate_child_tree(&mut child, &process_job);
        let _ = child.wait();
        return ToolResult::failed(
            request,
            format!("failed to configure shell output readers: {error}"),
            None,
        );
    }
    let reader_stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::sync_channel(STREAM_OUTPUT_CHANNEL_CAPACITY);
    let stdout_handle = stdout.map(|stdout| {
        let tx = tx.clone();
        let stop = Arc::clone(&reader_stop);
        std::thread::spawn(move || stream_pipe(stdout, tx, StreamEvent::Stdout, stop))
    });
    let stderr_handle = stderr.map(|stderr| {
        let tx = tx.clone();
        let stop = Arc::clone(&reader_stop);
        std::thread::spawn(move || stream_pipe(stderr, tx, StreamEvent::Stderr, stop))
    });
    drop(tx);

    let mut stdout = RetainedOutput::new(process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM);
    let mut stderr = RetainedOutput::new(process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM);
    let mut preview_remaining = STREAM_LIVE_PREVIEW_BYTES;
    let deadline = Instant::now()
        .checked_add(shell_timeout)
        .unwrap_or_else(Instant::now);
    let mut timed_out = false;
    let mut cancelled = false;
    let mut output_open = true;
    let status = loop {
        if output_open {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(StreamEvent::Stdout(chunk)) => {
                    emit_live_preview(&chunk, &mut preview_remaining, on_output);
                    stdout.append(&chunk);
                }
                Ok(StreamEvent::Stderr(chunk)) => {
                    emit_live_preview(&chunk, &mut preview_remaining, on_output);
                    stderr.append(&chunk);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => output_open = false,
            }
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                process::terminate_child_tree(&mut child, &process_job);
                break Ok(status);
            }
            Ok(None) => {}
            Err(error) => {
                return ToolResult::failed(
                    request,
                    format!("failed to wait for shell command: {error}"),
                    None,
                );
            }
        }
        if should_cancel() {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(error) => {
                    return ToolResult::failed(
                        request,
                        format!("failed to wait for shell command: {error}"),
                        None,
                    );
                }
            }
            cancelled = true;
            process::terminate_child_tree(&mut child, &process_job);
            break child
                .wait()
                .map_err(|error| format!("failed to wait for shell command: {error}"));
        }
        if Instant::now() >= deadline {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(error) => {
                    return ToolResult::failed(
                        request,
                        format!("failed to wait for shell command: {error}"),
                        None,
                    );
                }
            }
            timed_out = true;
            process::terminate_child_tree(&mut child, &process_job);
            break child
                .wait()
                .map_err(|error| format!("failed to wait for shell command: {error}"));
        }
    };

    reader_stop.store(true, Ordering::Release);
    while let Ok(event) = rx.recv() {
        match event {
            StreamEvent::Stdout(chunk) => {
                emit_live_preview(&chunk, &mut preview_remaining, on_output);
                stdout.append(&chunk);
            }
            StreamEvent::Stderr(chunk) => {
                emit_live_preview(&chunk, &mut preview_remaining, on_output);
                stderr.append(&chunk);
            }
        }
    }
    let stdout_reader = join_stream_reader(stdout_handle, "stdout");
    let stderr_reader = join_stream_reader(stderr_handle, "stderr");
    let status = match status {
        Ok(status) => status,
        Err(error) => return ToolResult::failed(request, error, None),
    };

    if let Err(error) = stdout_reader.and(stderr_reader) {
        return ToolResult::failed(request, error, status.code());
    }

    let stdout = stdout.into_snapshot();
    let stderr = stderr.into_snapshot();
    let stdout_omitted_bytes = stdout.omitted_bytes;
    let stderr_omitted_bytes = stderr.omitted_bytes;
    let ingress_truncated = stdout.is_truncated() || stderr.is_truncated();
    let stdout = String::from_utf8_lossy(&stdout.rendered_bytes())
        .trim_end()
        .to_string();
    let stderr = String::from_utf8_lossy(&stderr.rendered_bytes())
        .trim_end()
        .to_string();
    if status.success() && !timed_out && !cancelled {
        let (stdout, truncated) = truncate_output_with_policy(stdout, output_truncation);
        let stdout = process::preserve_ingress_omission_notice(stdout, stdout_omitted_bytes);
        ToolResult::completed(request, stdout, ingress_truncated || truncated)
    } else {
        let message = if cancelled {
            cancelled_message(&stdout, &stderr)
        } else if timed_out {
            timeout_message(shell_timeout, &stdout, &stderr)
        } else if stderr.is_empty() {
            stdout
        } else if stdout.is_empty() {
            stderr
        } else {
            format!("{stdout}\n{stderr}")
        };
        let (message, truncated) = truncate_output_with_policy(message, output_truncation);
        let message = process::preserve_ingress_omission_notice(
            message,
            stdout_omitted_bytes.saturating_add(stderr_omitted_bytes),
        );
        let mut result = if cancelled {
            ToolResult::cancelled(request, message, status.code())
        } else {
            ToolResult::failed(request, message, status.code())
        };
        result.set_truncated(ingress_truncated || truncated);
        result
    }
}

fn stream_pipe(
    mut pipe: impl Read,
    tx: mpsc::SyncSender<StreamEvent>,
    event: fn(Vec<u8>) -> StreamEvent,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut buffer = [0_u8; STREAM_OUTPUT_READ_CHUNK_BYTES];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                if tx.send(event(buffer[..read].to_vec())).is_err() {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn join_stream_reader(
    handle: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    stream: &str,
) -> Result<(), String> {
    let Some(handle) = handle else {
        return Ok(());
    };
    handle
        .join()
        .map_err(|_| format!("{stream} reader thread panicked"))?
        .map_err(|error| format!("failed to read shell {stream}: {error}"))
}

fn emit_live_preview(bytes: &[u8], remaining: &mut usize, on_output: &mut dyn FnMut(&str)) {
    if *remaining == 0 {
        return;
    }
    let admitted = bytes.len().min(*remaining);
    if admitted > 0 {
        on_output(&String::from_utf8_lossy(&bytes[..admitted]));
        *remaining -= admitted;
    }
}

fn cancelled_message(stdout: &str, stderr: &str) -> String {
    if stderr.is_empty() && stdout.is_empty() {
        "shell command cancelled".to_string()
    } else if stderr.is_empty() {
        format!("shell command cancelled: {stdout}")
    } else if stdout.is_empty() {
        format!("shell command cancelled: {stderr}")
    } else {
        format!("shell command cancelled: {stdout}\n{stderr}")
    }
}

fn timeout_message(shell_timeout: Duration, stdout: &str, stderr: &str) -> String {
    if stderr.is_empty() && stdout.is_empty() {
        format!("shell command timed out after {}s", shell_timeout.as_secs())
    } else if stderr.is_empty() {
        format!(
            "shell command timed out after {}s: {stdout}",
            shell_timeout.as_secs()
        )
    } else if stdout.is_empty() {
        format!(
            "shell command timed out after {}s: {stderr}",
            shell_timeout.as_secs()
        )
    } else {
        format!(
            "shell command timed out after {}s: {stdout}\n{stderr}",
            shell_timeout.as_secs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    #[test]
    fn streaming_reader_io_failure_is_returned() {
        let handle = std::thread::spawn(|| Err(std::io::Error::other("reader failed")));

        let error = join_stream_reader(Some(handle), "stdout").unwrap_err();

        assert!(error.contains("failed to read shell stdout"));
        assert!(error.contains("reader failed"));
    }

    #[test]
    fn streaming_reader_panic_is_returned() {
        let handle = std::thread::spawn(|| -> std::io::Result<()> { panic!("reader panicked") });

        let error = join_stream_reader(Some(handle), "stderr").unwrap_err();

        assert!(error.contains("stderr reader thread panicked"));
    }
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolStatus};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Instant;

    fn sandbox_enforcement_available() -> bool {
        crate::sandbox::enforcement_state() == orca_core::capability::EnforcementState::Enforced
    }

    fn bash_request(command: &str) -> ToolRequest {
        ToolRequest {
            id: "bash-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(command.to_string()),
            raw_arguments: None,
        }
    }

    fn platform_script(unix: impl Into<String>, windows: impl Into<String>) -> String {
        if cfg!(windows) {
            windows.into()
        } else {
            unix.into()
        }
    }

    fn platform_delay(unix_ms: u64, windows_ms: u64) -> Duration {
        Duration::from_millis(if cfg!(windows) { windows_ms } else { unix_ms })
    }

    fn platform_test_deadline(unix_secs: u64, windows_secs: u64) -> Duration {
        Duration::from_secs(if cfg!(windows) {
            windows_secs
        } else {
            unix_secs
        })
    }

    fn host_test_command(script: &str, cwd: &Path) -> std::process::Command {
        let shell = orca_platform::shell::ShellResolver::for_current_host()
            .resolve_from_environment()
            .expect("resolve host test shell");
        let mut command = process::shell_command(&shell, script);
        command.current_dir(cwd);
        command
    }

    fn execute_host_test_with_policy_or_cancel(
        request: &ToolRequest,
        cwd: &Path,
        output_truncation: ToolOutputTruncation,
        shell_timeout: Duration,
        should_cancel: impl Fn() -> bool,
    ) -> ToolResult {
        execute_command_with_policy_or_cancel(
            request,
            host_test_command(request.target.as_deref().unwrap_or_default(), cwd),
            cwd,
            output_truncation,
            shell_timeout,
            should_cancel,
        )
    }

    fn execute_host_test_streaming_with_policy_or_cancel(
        request: &ToolRequest,
        cwd: &Path,
        output_truncation: ToolOutputTruncation,
        shell_timeout: Duration,
        on_output: &mut dyn FnMut(&str),
        should_cancel: impl Fn() -> bool,
    ) -> ToolResult {
        execute_streaming_command_or_cancel(
            request,
            host_test_command(request.target.as_deref().unwrap_or_default(), cwd),
            cwd,
            output_truncation,
            shell_timeout,
            on_output,
            should_cancel,
        )
    }

    #[test]
    fn streaming_reports_output_chunks_and_final_result() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let command = platform_script(
            "printf 'one\\ntwo\\n'",
            "[Console]::Out.Write(\"one`ntwo`n\"); [Console]::Out.Flush()",
        );
        let request = bash_request(&command);
        let mut chunks = Vec::new();

        let result = execute_host_test_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(120),
            &mut |chunk| chunks.push(chunk.to_string()),
            || false,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("one\ntwo"));
        let joined = chunks.join("");
        assert!(joined.contains("one\n"), "expected stdout in chunks");
        assert!(joined.contains("two\n"), "expected stdout in chunks");
    }

    #[test]
    fn streaming_large_unterminated_output_is_bounded_before_result_truncation() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let logical_bytes = process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM * 2;
        let command = platform_script(
            format!(
                "printf HEAD; yes x | tr -d '\\n' | head -c {}; printf TAIL",
                logical_bytes - 8
            ),
            format!(
                "[Console]::Out.Write('HEAD'); [Console]::Out.Write('x' * {}); [Console]::Out.Write('TAIL'); [Console]::Out.Flush()",
                logical_bytes - 8
            ),
        );
        let request = bash_request(&command);
        let mut streamed_bytes = 0usize;

        let result = execute_host_test_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(4096),
            Duration::from_secs(10),
            &mut |chunk| streamed_bytes = streamed_bytes.saturating_add(chunk.len()),
            || false,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.truncated);
        let output = result.output.as_deref().expect("bounded output");
        assert!(
            output.starts_with("HEAD"),
            "missing stable prefix: {output}"
        );
        assert!(output.ends_with("TAIL"), "missing rolling suffix: {output}");
        assert!(
            output.contains("omitted"),
            "missing omission marker: {output}"
        );
        assert!(
            streamed_bytes <= process::DEFAULT_PROCESS_OUTPUT_RETAINED_BYTES_PER_STREAM,
            "live callback admitted {streamed_bytes} bytes"
        );
    }

    #[test]
    fn bash_commands_receive_eof_on_stdin_instead_of_inheriting_terminal() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let command = platform_script(
            "read line; printf done",
            "$null = [Console]::In.ReadLine(); [Console]::Out.Write('done')",
        );
        let request = bash_request(&command);
        let start = Instant::now();

        let result = execute_host_test_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            platform_test_deadline(2, 5),
            || false,
        );

        assert!(
            start.elapsed() < platform_test_deadline(1, 4),
            "stdin should be closed without waiting for timeout"
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("done"));
    }

    #[test]
    fn streaming_respects_shell_timeout_and_returns_partial_output() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let command = platform_script(
            "printf before; sleep 5; printf after",
            "[Console]::Out.Write('before'); [Console]::Out.Flush(); Start-Sleep -Seconds 5; [Console]::Out.Write('after')",
        );
        let request = bash_request(&command);
        let mut chunks = Vec::new();
        let start = Instant::now();

        let shell_timeout = platform_delay(200, 1_500);
        let result = execute_host_test_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            shell_timeout,
            &mut |chunk| chunks.push(chunk.to_string()),
            || false,
        );

        assert!(
            start.elapsed() < Duration::from_secs(4),
            "streaming command should not wait for the child to finish"
        );
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains(&format!(
                    "shell command timed out after {}s",
                    shell_timeout.as_secs()
                )),
            "unexpected error: {:?}",
            result.error
        );
        assert!(
            chunks.join("").contains("before"),
            "partial output should be streamed before timeout"
        );
    }

    #[test]
    fn noisy_streaming_timeout_does_not_deadlock_reader_shutdown() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let command = platform_script(
            "while :; do printf 1234567890; done",
            "while ($true) { [Console]::Out.Write('1234567890') }",
        );
        let request = bash_request(&command);
        let start = Instant::now();
        let mut delayed_callback = false;

        let shell_timeout = platform_delay(100, 1_500);
        let result = execute_host_test_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            shell_timeout,
            &mut |_| {
                if !delayed_callback {
                    delayed_callback = true;
                    std::thread::sleep(Duration::from_millis(250));
                }
            },
            || false,
        );

        assert!(
            start.elapsed() < Duration::from_secs(4),
            "noisy streaming timeout deadlocked reader shutdown: {:?}",
            start.elapsed()
        );
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains(&format!(
                    "shell command timed out after {}s",
                    shell_timeout.as_secs()
                )),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[test]
    #[cfg(unix)]
    fn escaped_session_descendant_cannot_deadlock_stream_shutdown() {
        if !sandbox_enforcement_available() {
            return;
        }
        let helper = std::env::current_exe().expect("resolve test executable");
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(
                "\"$ORCA_BASH_ESCAPE_HELPER\" --exact bash::tests::escaped_stream_pipe_holder_helper --nocapture & printf parent-done",
            )
            .env("ORCA_BASH_ESCAPE_HELPER", helper)
            .env("ORCA_BASH_ESCAPE_HOLDER", "1")
            .process_group(0);
        let request = bash_request("escaped pipe holder");
        let started = Instant::now();

        let result = execute_streaming_command_or_cancel(
            &request,
            command,
            &std::env::current_dir().expect("current directory"),
            ToolOutputTruncation::bytes(1024),
            Duration::from_millis(200),
            &mut |_| {},
            || false,
        );

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "escaped streaming reader exceeded deadline: {:?}",
            started.elapsed()
        );
        assert_eq!(result.status, ToolStatus::Completed);
        assert!(
            result
                .output
                .as_deref()
                .is_some_and(|output| output.contains("parent-done"))
        );
        assert!(result.error.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn escaped_stream_pipe_holder_helper() {
        if std::env::var_os("ORCA_BASH_ESCAPE_HOLDER").is_none() {
            return;
        }
        unsafe {
            libc::setsid();
        }
        std::thread::sleep(Duration::from_secs(5));
    }

    #[test]
    fn bash_wait_observes_cancel_callback() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let command = platform_script(
            "printf before; sleep 5; printf after",
            "[Console]::Out.Write('before'); [Console]::Out.Flush(); Start-Sleep -Seconds 5; [Console]::Out.Write('after')",
        );
        let request = bash_request(&command);
        let start = Instant::now();

        let result = execute_host_test_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            || start.elapsed() >= platform_delay(100, 1_500),
        );

        assert!(
            start.elapsed() < Duration::from_secs(4),
            "cancelled command should not wait for the shell timeout"
        );
        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(
            result.kind,
            orca_core::tool_types::ToolResultKind::Cancelled
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("shell command cancelled"),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[test]
    fn bash_wait_preserves_one_shot_cancel_observation() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let cancel_ready = dir.path().join("cancel-ready");
        let command = platform_script(
            "printf before; : > cancel-ready; sleep 5; printf after",
            "[Console]::Out.Write('before'); [Console]::Out.Flush(); New-Item -ItemType File -Force -Path 'cancel-ready' | Out-Null; Start-Sleep -Seconds 5; [Console]::Out.Write('after')",
        );
        let request = bash_request(&command);
        let cancellation_delivered = std::cell::Cell::new(false);

        let result = execute_host_test_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            || cancel_ready.exists() && !cancellation_delivered.replace(true),
        );

        assert_eq!(result.status, ToolStatus::Cancelled);
        assert!(cancellation_delivered.get());
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("before"))
        );
    }

    #[test]
    fn streaming_bash_wait_observes_cancel_callback() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let command = platform_script(
            "printf 'before\\n'; sleep 5; printf after",
            "[Console]::Out.Write(\"before`n\"); [Console]::Out.Flush(); Start-Sleep -Seconds 5; [Console]::Out.Write('after')",
        );
        let request = bash_request(&command);
        let mut chunks = Vec::new();
        let start = Instant::now();
        let saw_output = Arc::new(AtomicBool::new(false));
        let saw_output_for_chunk = Arc::clone(&saw_output);
        let saw_output_for_cancel = Arc::clone(&saw_output);

        let result = execute_host_test_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            &mut |chunk| {
                if chunk.contains("before") {
                    saw_output_for_chunk.store(true, Ordering::SeqCst);
                }
                chunks.push(chunk.to_string());
            },
            || {
                (saw_output_for_cancel.load(Ordering::SeqCst)
                    && start.elapsed() >= platform_delay(100, 1_500))
                    || start.elapsed() >= platform_delay(1_000, 3_000)
            },
        );

        assert!(
            start.elapsed() < Duration::from_secs(4),
            "cancelled streaming command should not wait for the shell timeout"
        );
        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(
            result.kind,
            orca_core::tool_types::ToolResultKind::Cancelled
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("shell command cancelled"),
            "unexpected error: {:?}",
            result.error
        );
        assert!(
            chunks.join("").contains("before"),
            "partial output should still stream before cancellation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn streaming_completed_process_wins_cancellation_observed_during_callback() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let release = dir.path().join("release");
        let completed = dir.path().join("completed");
        let request = bash_request(&format!(
            "while [ ! -e {release:?} ]; do sleep 0.01; done; printf 'completed\\n'; : > {completed:?}"
        ));
        let cancellation_observed = AtomicBool::new(false);
        let mut chunks = Vec::new();

        let result = execute_host_test_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(5),
            &mut |chunk| chunks.push(chunk.to_string()),
            || {
                if !cancellation_observed.swap(true, Ordering::SeqCst) {
                    std::fs::write(&release, []).expect("release child");
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !completed.exists() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    assert!(completed.exists(), "child did not complete during callback");
                    std::thread::sleep(Duration::from_millis(50));
                }
                true
            },
        );

        assert!(cancellation_observed.load(Ordering::SeqCst));
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("completed"));
        assert!(chunks.join("").contains("completed"));
    }

    #[cfg(unix)]
    #[test]
    fn streaming_bash_keeps_polling_cancel_after_output_closes() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let request = bash_request("exec >/dev/null 2>/dev/null; sleep 5");
        let started = Instant::now();
        let mut output = String::new();

        let result = execute_host_test_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            &mut |chunk| output.push_str(chunk),
            || started.elapsed() >= Duration::from_millis(100),
        );

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancelled command with closed output should not block in wait"
        );
        assert_eq!(result.status, ToolStatus::Cancelled, "{result:?}");
        assert!(output.is_empty());
    }

    #[test]
    fn noisy_streaming_cancel_does_not_deadlock_reader_shutdown() {
        if !sandbox_enforcement_available() {
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        let command = platform_script(
            "while :; do printf 1234567890; done",
            "while ($true) { [Console]::Out.Write('1234567890') }",
        );
        let request = bash_request(&command);
        let start = Instant::now();
        let mut delayed_callback = false;

        let result = execute_host_test_streaming_with_policy_or_cancel(
            &request,
            dir.path(),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(30),
            &mut |_| {
                if !delayed_callback {
                    delayed_callback = true;
                    std::thread::sleep(Duration::from_millis(250));
                }
            },
            || start.elapsed() >= platform_delay(100, 1_500),
        );

        assert!(
            start.elapsed() < Duration::from_secs(4),
            "noisy streaming cancel deadlocked reader shutdown: {:?}",
            start.elapsed()
        );
        assert_eq!(result.status, ToolStatus::Cancelled);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("shell command cancelled"),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[test]
    fn bash_command_allows_additional_working_directory_writes() {
        if !crate::sandbox::seatbelt_available() {
            return;
        }

        let parent = crate::sandbox::sandbox_test_parent("bash-additional-roots-");
        let workspace = parent.path().join("workspace");
        let extra = parent.path().join("extra");
        let outside = parent.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&extra).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let extra_file = extra.join("allowed.txt");
        let outside_file = outside.join("blocked.txt");
        let request = bash_request(&format!(
            "printf allowed > {} && printf blocked > {}",
            extra_file.display(),
            outside_file.display()
        ));
        let result = execute_with_policy_roots_or_cancel(
            &request,
            &workspace,
            std::slice::from_ref(&extra),
            ToolOutputTruncation::bytes(1024),
            Duration::from_secs(5),
            || false,
        );
        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(std::fs::read_to_string(extra_file).unwrap(), "allowed");
        assert!(!outside_file.exists());
    }
}
