use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::Value;

use super::JsonlCommandExecPermissionRequest;
use super::cap_text;
use crate::network_proxy::{RuntimeNetworkBlockReport, RuntimeNetworkProxy};
use crate::protocol::{self, ServerEvent};
use crate::runtime_permission::{
    RuntimePermissionDecision, RuntimePermissionEvaluation, RuntimePermissionOrigin,
    RuntimePermissionPolicy, RuntimePermissionRequestKind,
};
use crate::sandbox_denial::diagnose_sandbox_denial;
use crate::shell_session::RuntimeShellSessionManager;

pub(super) struct CommandExecProcess {
    pub(super) shell_id: Option<String>,
    pub(super) command_event_id: Value,
    pub(super) command: Vec<String>,
    pub(super) cwd: PathBuf,
    /// User-facing lexical cwd. `cwd` remains canonical and is used for
    /// enforcement/diagnostics; this value avoids leaking Windows verbatim
    /// path syntax through the protocol.
    pub(super) display_cwd: PathBuf,
    pub(super) stream_output: bool,
    pub(super) output_bytes_cap: Option<usize>,
    pub(super) output_offset: usize,
    pub(super) stdout_len: usize,
    pub(super) stderr_len: usize,
    pub(super) stdout_cap_reached: bool,
    pub(super) stderr_cap_reached: bool,
    pub(super) network_permission_blocks: Option<mpsc::Receiver<RuntimeNetworkBlockReport>>,
    pub(super) permission_request: Option<JsonlCommandExecPermissionRequest>,
    pub(super) _network_proxy: Option<RuntimeNetworkProxy>,
}

#[derive(Default)]
pub(super) struct CommandExecManager {
    processes: HashMap<String, CommandExecProcess>,
}

pub(super) struct CommandExecProcessSnapshot {
    pub(super) process_id: String,
    pub(super) shell_id: Option<String>,
    pub(super) command: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) status: &'static str,
    pub(super) stream_output: bool,
    pub(super) output_bytes_cap: Option<usize>,
    pub(super) stdout_bytes: usize,
    pub(super) stderr_bytes: usize,
}

pub(super) enum CommandExecDrainOutcome {
    Drained,
    NetworkPermissionRequired {
        request: JsonlCommandExecPermissionRequest,
        block: RuntimeNetworkBlockReport,
    },
    NetworkPermissionDenied {
        command_event_id: Value,
        reason: String,
    },
}

pub(super) struct CommandExecPermissionPrompt {
    pub(super) origin: RuntimePermissionOrigin,
    pub(super) kind: RuntimePermissionRequestKind,
    pub(super) reason: String,
    pub(super) permissions: protocol::RequestPermissionProfile,
}

pub(super) struct CommandExecPermissionDenial {
    pub(super) reason: String,
}

impl From<RuntimePermissionDecision> for CommandExecPermissionPrompt {
    fn from(decision: RuntimePermissionDecision) -> Self {
        Self {
            origin: decision.origin,
            kind: decision.kind,
            reason: decision.request.reason.unwrap_or_default(),
            permissions: decision.request.permissions,
        }
    }
}

impl CommandExecPermissionPrompt {
    pub(super) fn into_request_parts(
        self,
    ) -> (
        RuntimePermissionOrigin,
        RuntimePermissionRequestKind,
        String,
        protocol::RequestPermissionProfile,
    ) {
        (self.origin, self.kind, self.reason, self.permissions)
    }
}

pub(super) struct CommandExecPermissionPolicy;

impl CommandExecPermissionPolicy {
    pub(super) fn network_permission_block(
        blocked_hosts: mpsc::Receiver<RuntimeNetworkBlockReport>,
    ) -> Option<RuntimeNetworkBlockReport> {
        blocked_hosts.try_iter().next()
    }

    pub(super) fn network_block_prompt(
        block: &RuntimeNetworkBlockReport,
    ) -> Option<CommandExecPermissionPrompt> {
        RuntimePermissionPolicy::network_block_decision(
            "command-exec",
            RuntimePermissionOrigin::CommandExec,
            block,
        )
        .map(CommandExecPermissionPrompt::from)
    }

    pub(super) fn network_block_denial(
        block: &RuntimeNetworkBlockReport,
    ) -> Option<CommandExecPermissionDenial> {
        match RuntimePermissionPolicy::network_block_evaluation(
            "command-exec",
            RuntimePermissionOrigin::CommandExec,
            block,
        ) {
            RuntimePermissionEvaluation::Request(_) => None,
            RuntimePermissionEvaluation::Deny { reason, .. } => {
                Some(CommandExecPermissionDenial { reason })
            }
        }
    }
}

impl CommandExecManager {
    pub(super) fn insert(
        &mut self,
        process_id: String,
        process: CommandExecProcess,
    ) -> Result<(), String> {
        if self.processes.contains_key(&process_id) {
            return Err(format!(
                "duplicate active command/exec process id: {:?}",
                process_id
            ));
        }
        self.processes.insert(process_id, process);
        Ok(())
    }

    pub(super) fn activate(&mut self, process_id: &str, shell_id: String) -> bool {
        let Some(process) = self.processes.get_mut(process_id) else {
            return false;
        };
        process.shell_id = Some(shell_id);
        true
    }

    pub(super) fn retain_network_proxy(&mut self, process_id: &str, proxy: RuntimeNetworkProxy) {
        if let Some(process) = self.processes.get_mut(process_id) {
            process._network_proxy = Some(proxy);
        }
    }

    pub(super) fn get(&self, process_id: &str) -> Option<&CommandExecProcess> {
        self.processes.get(process_id)
    }

    fn get_mut(&mut self, process_id: &str) -> Option<&mut CommandExecProcess> {
        self.processes.get_mut(process_id)
    }

    pub(super) fn remove(&mut self, process_id: &str) -> Option<CommandExecProcess> {
        self.processes.remove(process_id)
    }

    pub(super) fn active_shell_ids(&self) -> HashSet<String> {
        self.processes
            .values()
            .filter_map(|process| process.shell_id.clone())
            .collect()
    }

    pub(super) fn tighten_output_cap(&mut self, process_id: &str, output_bytes_cap: usize) {
        if let Some(process) = self.get_mut(process_id) {
            process.output_bytes_cap = Some(
                process
                    .output_bytes_cap
                    .map(|existing| existing.min(output_bytes_cap))
                    .unwrap_or(output_bytes_cap),
            );
        }
    }

    fn process_ids(&self) -> Vec<String> {
        self.processes.keys().cloned().collect()
    }

    pub(super) fn list(&self) -> Vec<CommandExecProcessSnapshot> {
        let mut process_ids = self.process_ids();
        process_ids.sort();
        process_ids
            .into_iter()
            .filter_map(|process_id| {
                self.processes
                    .get(&process_id)
                    .map(|process| CommandExecProcessSnapshot {
                        process_id,
                        shell_id: process.shell_id.clone(),
                        command: process.command.clone(),
                        cwd: process.display_cwd.clone(),
                        status: if process.shell_id.is_some() {
                            "running"
                        } else {
                            "starting"
                        },
                        stream_output: process.stream_output,
                        output_bytes_cap: process.output_bytes_cap,
                        stdout_bytes: process.stdout_len,
                        stderr_bytes: process.stderr_len,
                    })
            })
            .collect()
    }

    pub(super) fn write_to_process<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        process_id: &str,
        delta_base64: Option<&str>,
        close_stdin: bool,
        id: &Value,
        writer: &mut W,
    ) -> io::Result<()> {
        let Some(process) = self.get(process_id) else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        let Some(shell_id) = process.shell_id.clone() else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("command process is still starting: {process_id}")),
            );
        };
        let Some(manager) = shell_sessions else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        if let Some(delta_base64) = delta_base64 {
            let bytes = match BASE64_STANDARD.decode(delta_base64) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return protocol::write_server_event(
                        writer,
                        id,
                        ServerEvent::error(format!(
                            "invalid command/exec write deltaBase64: {error}"
                        )),
                    );
                }
            };
            let input = String::from_utf8_lossy(&bytes);
            if let Err(error) = manager.write_stdin(&shell_id, &input) {
                return protocol::write_server_event(
                    writer,
                    id,
                    ServerEvent::error(error.to_string()),
                );
            }
        }
        if close_stdin && let Err(error) = manager.close_stdin(&shell_id) {
            return protocol::write_server_event(writer, id, ServerEvent::error(error.to_string()));
        }
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::CommandExecWritten {
                process_id: Value::from(process_id.to_string()),
            },
        )?;
        self.drain_with_timeout(Some(manager), writer, Duration::from_secs(5))
            .map(|_| ())
    }

    pub(super) fn read_process<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        process_id: &str,
        timeout: Duration,
        output_bytes_cap: Option<usize>,
        id: &Value,
        writer: &mut W,
    ) -> io::Result<CommandExecDrainOutcome> {
        let Some(process) = self.get(process_id) else {
            protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            )?;
            return Ok(CommandExecDrainOutcome::Drained);
        };
        if process.shell_id.is_none() {
            protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("command process is still starting: {process_id}")),
            )?;
            return Ok(CommandExecDrainOutcome::Drained);
        }
        let Some(manager) = shell_sessions else {
            protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            )?;
            return Ok(CommandExecDrainOutcome::Drained);
        };
        if let Some(output_bytes_cap) = output_bytes_cap {
            self.tighten_output_cap(process_id, output_bytes_cap);
        }
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::CommandExecRead {
                process_id: Value::from(process_id.to_string()),
                status: Value::from("running"),
            },
        )?;
        self.drain_until_output_or_timeout(Some(manager), writer, timeout)
    }

    pub(super) fn resize_process<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        process_id: &str,
        cols: u16,
        rows: u16,
        id: &Value,
        writer: &mut W,
    ) -> io::Result<()> {
        let Some(process) = self.get(process_id) else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        let Some(shell_id) = process.shell_id.clone() else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("command process is still starting: {process_id}")),
            );
        };
        let Some(manager) = shell_sessions else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        if let Err(error) = manager.resize(&shell_id, cols, rows) {
            return protocol::write_server_event(writer, id, ServerEvent::error(error.to_string()));
        }
        protocol::write_server_event(
            writer,
            id,
            ServerEvent::CommandExecResized {
                process_id: Value::from(process_id.to_string()),
                cols: Value::from(cols),
                rows: Value::from(rows),
            },
        )
    }

    pub(super) fn drain<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        writer: &mut W,
    ) -> io::Result<CommandExecDrainOutcome> {
        self.drain_with_timeout(shell_sessions, writer, Duration::from_millis(1))
    }

    pub(super) fn drain_with_timeout<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        writer: &mut W,
        timeout: Duration,
    ) -> io::Result<CommandExecDrainOutcome> {
        self.drain_inner(shell_sessions, writer, timeout, false)
    }

    pub(super) fn drain_until_output_or_timeout<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        writer: &mut W,
        timeout: Duration,
    ) -> io::Result<CommandExecDrainOutcome> {
        self.drain_inner(shell_sessions, writer, timeout, true)
    }

    fn drain_inner<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        writer: &mut W,
        timeout: Duration,
        return_on_output: bool,
    ) -> io::Result<CommandExecDrainOutcome> {
        let Some(manager) = shell_sessions else {
            return Ok(CommandExecDrainOutcome::Drained);
        };
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        loop {
            let process_ids = self.process_ids();
            if process_ids.is_empty() {
                return Ok(CommandExecDrainOutcome::Drained);
            }
            let mut observed_output = false;
            for process_id in process_ids {
                let Some(shell_id) = self
                    .get(&process_id)
                    .and_then(|process| process.shell_id.clone())
                else {
                    continue;
                };
                let mut output =
                    match manager.read_preserving_output(&shell_id, Duration::from_millis(1)) {
                        Ok(output) => output,
                        Err(_) => continue,
                    };
                let Some(process) = self.get(&process_id) else {
                    continue;
                };
                if process.stream_output {
                    let output_read = manager
                        .read_output_delta(&output.task_id, process.output_offset, usize::MAX)
                        .unwrap_or_else(|_| crate::task_output::TaskOutputRead {
                            stdout: String::new(),
                            stderr: String::new(),
                            combined: String::new(),
                            next_offset: process.output_offset,
                            bytes_read: 0,
                            bytes_total: process.output_offset,
                            omitted_prefix_bytes: 0,
                            stdout_prefix_bytes: process.stdout_len,
                            stderr_prefix_bytes: process.stderr_len,
                        });
                    let omitted_notice =
                        stream_omitted_prefix_notice(output_read.omitted_prefix_bytes);
                    let stdout_omitted_notice = (!output_read.stdout.is_empty())
                        .then_some(omitted_notice.as_deref())
                        .flatten();
                    let stderr_omitted_notice = output_read
                        .stdout
                        .is_empty()
                        .then_some(omitted_notice.as_deref())
                        .flatten();
                    let stdout_delta = capped_stream_delta(
                        &output_read.stdout,
                        stdout_omitted_notice,
                        output_read.stdout_prefix_bytes,
                        process.output_bytes_cap,
                        process.stdout_cap_reached,
                    );
                    let stderr_delta = capped_stream_delta(
                        &output_read.stderr,
                        stderr_omitted_notice,
                        output_read.stderr_prefix_bytes,
                        process.output_bytes_cap,
                        process.stderr_cap_reached,
                    );
                    observed_output |=
                        !stdout_delta.text.is_empty() || !stderr_delta.text.is_empty();
                    super::write_command_exec_output_deltas(
                        writer,
                        &process_id,
                        &stdout_delta.text,
                        &stderr_delta.text,
                        stdout_delta.cap_reached,
                        stderr_delta.cap_reached,
                        output.status != orca_core::task_types::TaskStatus::Running,
                    )?;
                    if let Some(process) = self.get_mut(&process_id) {
                        process.output_offset = output_read.next_offset;
                        process.stdout_len =
                            process.stdout_len.saturating_add(stdout_delta.output_bytes);
                        process.stderr_len =
                            process.stderr_len.saturating_add(stderr_delta.output_bytes);
                        process.stdout_cap_reached |= stdout_delta.cap_reached;
                        process.stderr_cap_reached |= stderr_delta.cap_reached;
                    }
                }
                if output.status != orca_core::task_types::TaskStatus::Running {
                    let Some(process) = self.remove(&process_id) else {
                        continue;
                    };
                    if let Some(block) = process
                        .network_permission_blocks
                        .and_then(CommandExecPermissionPolicy::network_permission_block)
                    {
                        if let Some(denial) =
                            CommandExecPermissionPolicy::network_block_denial(&block)
                        {
                            manager.remove_output(&output.task_id);
                            return Ok(CommandExecDrainOutcome::NetworkPermissionDenied {
                                command_event_id: process.command_event_id,
                                reason: denial.reason,
                            });
                        }
                        if let Some(request) = process.permission_request {
                            manager.remove_output(&output.task_id);
                            return Ok(CommandExecDrainOutcome::NetworkPermissionRequired {
                                request,
                                block,
                            });
                        }
                    }
                    if let Some(diagnostic) =
                        diagnose_sandbox_denial(&process.cwd, &output.stdout, &output.stderr)
                    {
                        // stderr is explanatory only. A process-controlled
                        // denial string is not a kernel receipt and cannot
                        // mint a filesystem or shell escalation request.
                        if output.stderr.trim_end().is_empty() {
                            output.stderr = diagnostic.message;
                        } else {
                            output.stderr.push_str("\n\nSandbox diagnostic: ");
                            output.stderr.push_str(&diagnostic.message);
                        }
                    }
                    protocol::write_server_event(
                        writer,
                        &process.command_event_id,
                        ServerEvent::CommandExecCompleted {
                            process_id: Value::from(process_id),
                            exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
                            stdout: if process.stream_output {
                                Value::from("")
                            } else {
                                Value::from(cap_text(&output.stdout, process.output_bytes_cap))
                            },
                            stderr: if process.stream_output {
                                Value::from("")
                            } else {
                                Value::from(cap_text(&output.stderr, process.output_bytes_cap))
                            },
                        },
                    )?;
                    manager.remove_output(&output.task_id);
                }
            }
            if std::time::Instant::now() >= deadline
                || (observed_output && (return_on_output || timeout <= Duration::from_millis(1)))
            {
                return Ok(CommandExecDrainOutcome::Drained);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn terminate_process<W: Write>(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
        process_id: &str,
        id: &Value,
        writer: &mut W,
    ) -> io::Result<()> {
        let Some(process) = self.get(process_id) else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        let Some(shell_id) = process.shell_id.clone() else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("command process is still starting: {process_id}")),
            );
        };
        let Some(manager) = shell_sessions else {
            return protocol::write_server_event(
                writer,
                id,
                ServerEvent::error(format!("unknown command process: {process_id}")),
            );
        };
        match manager.kill(&shell_id) {
            Ok(output) => {
                let Some(process) = self.remove(process_id) else {
                    return protocol::write_server_event(
                        writer,
                        id,
                        ServerEvent::error(format!("unknown command process: {process_id}")),
                    );
                };
                protocol::write_server_event(
                    writer,
                    id,
                    ServerEvent::CommandExecTerminated {
                        process_id: Value::from(process_id.to_string()),
                    },
                )?;
                protocol::write_server_event(
                    writer,
                    &process.command_event_id,
                    ServerEvent::CommandExecCompleted {
                        process_id: Value::from(process_id.to_string()),
                        exit_code: output.exit_code.map(Value::from).unwrap_or(Value::Null),
                        stdout: if process.stream_output {
                            Value::from("")
                        } else {
                            Value::from(cap_text(&output.stdout, process.output_bytes_cap))
                        },
                        stderr: if process.stream_output {
                            Value::from("")
                        } else {
                            Value::from(cap_text(&output.stderr, process.output_bytes_cap))
                        },
                    },
                )
            }
            Err(error) => {
                protocol::write_server_event(writer, id, ServerEvent::error(error.to_string()))
            }
        }
    }

    pub(super) fn terminate_all(
        &mut self,
        shell_sessions: Option<&mut RuntimeShellSessionManager>,
    ) {
        let Some(manager) = shell_sessions else {
            self.processes.clear();
            return;
        };
        for process_id in self.process_ids() {
            let Some(process) = self.remove(&process_id) else {
                continue;
            };
            let Some(shell_id) = process.shell_id else {
                continue;
            };
            let _ = manager.kill(&shell_id);
        }
    }
}

struct CappedStreamDelta {
    text: String,
    output_bytes: usize,
    cap_reached: bool,
}

fn capped_stream_delta(
    delta: &str,
    notice: Option<&str>,
    sent_len: usize,
    cap: Option<usize>,
    cap_already_reached: bool,
) -> CappedStreamDelta {
    let notice = notice.unwrap_or("");
    let Some(cap) = cap else {
        return CappedStreamDelta {
            text: format!("{notice}{delta}"),
            output_bytes: delta.len(),
            cap_reached: false,
        };
    };
    if cap_already_reached {
        return CappedStreamDelta {
            text: String::new(),
            output_bytes: 0,
            cap_reached: false,
        };
    }
    let remaining = cap.saturating_sub(sent_len);
    let capped = cap_text(delta, Some(remaining));
    CappedStreamDelta {
        text: format!("{notice}{capped}"),
        output_bytes: capped.len(),
        cap_reached: delta.len() >= remaining,
    }
}

fn stream_omitted_prefix_notice(omitted_prefix_bytes: usize) -> Option<String> {
    (omitted_prefix_bytes > 0)
        .then(|| format!("[{omitted_prefix_bytes} bytes of earlier output omitted]\n"))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{CommandExecManager, CommandExecPermissionPolicy, CommandExecProcess};
    use crate::network_proxy::RuntimeNetworkBlockReport;
    use crate::runtime_permission::{RuntimePermissionOrigin, RuntimePermissionRequestKind};
    use crate::shell_session::{
        RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellTerminalMode,
    };
    use crate::task_output::TaskOutputStore;
    use crate::tasks::TaskRegistry;
    use serde_json::Value;

    fn platform_shell_script(unix: &str, windows: &str) -> String {
        if cfg!(windows) {
            windows.to_string()
        } else {
            unix.to_string()
        }
    }

    fn platform_command_argv() -> Vec<String> {
        if cfg!(windows) {
            vec!["pwsh.exe".to_string(), "-Command".to_string()]
        } else {
            vec!["sh".to_string(), "-lc".to_string()]
        }
    }

    #[test]
    fn command_exec_permission_policy_builds_network_prompt_for_allowlist_block() {
        let block = RuntimeNetworkBlockReport {
            host: "api.orca.invalid".to_string(),
            error: "blocked-by-allowlist",
        };

        let prompt =
            CommandExecPermissionPolicy::network_block_prompt(&block).expect("prompt request");

        assert_eq!(prompt.origin, RuntimePermissionOrigin::CommandExec);
        assert_eq!(prompt.kind, RuntimePermissionRequestKind::NetworkBlock);
        assert_eq!(
            prompt.reason,
            "command/exec attempted network access to api.orca.invalid (blocked-by-allowlist)"
        );
        assert_eq!(
            prompt
                .permissions
                .network
                .expect("network permissions")
                .domains
                .get("api.orca.invalid"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
        );
    }

    #[test]
    fn command_exec_permission_policy_does_not_prompt_for_denylist_block() {
        let block = RuntimeNetworkBlockReport {
            host: "blocked.orca.invalid".to_string(),
            error: "blocked-by-denylist",
        };

        assert!(CommandExecPermissionPolicy::network_block_prompt(&block).is_none());
    }

    #[test]
    fn command_exec_permission_policy_explains_network_denylist_blocks() {
        let block = RuntimeNetworkBlockReport {
            host: "blocked.orca.invalid".to_string(),
            error: "blocked-by-denylist",
        };

        let denial =
            CommandExecPermissionPolicy::network_block_denial(&block).expect("policy denial");

        assert_eq!(
            denial.reason,
            "command/exec network access to blocked.orca.invalid was denied by configured network policy"
        );
    }

    #[test]
    fn command_exec_permission_policy_keeps_denylist_blocks_for_final_denial() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(RuntimeNetworkBlockReport {
                host: "blocked.orca.invalid".to_string(),
                error: "blocked-by-denylist",
            })
            .expect("send block");
        drop(sender);

        let block = CommandExecPermissionPolicy::network_permission_block(receiver)
            .expect("network block should reach command/exec policy");

        assert_eq!(block.host, "blocked.orca.invalid");
        assert!(CommandExecPermissionPolicy::network_block_prompt(&block).is_none());
        assert!(CommandExecPermissionPolicy::network_block_denial(&block).is_some());
    }

    #[test]
    fn streaming_delta_survives_retained_output_rebase() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-output-rebase".to_string());
        let output_store = TaskOutputStore::with_max_retained_bytes(5);
        let mut shell_sessions =
            RuntimeShellSessionManager::with_output_store(task_registry, output_store);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: platform_shell_script(
                    "printf first; sleep 0.2; printf later; sleep 0.2",
                    "[Console]::Out.Write('first'); Start-Sleep -Milliseconds 200; [Console]::Out.Write('later'); Start-Sleep -Milliseconds 200",
                ),
                argv: None,
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "stream output across retained tail rebase".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-rebase".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id),
                    command_event_id: Value::from("cmd-rebase"),
                    command: platform_command_argv(),
                    cwd: cwd.path().to_path_buf(),
                    display_cwd: cwd.path().to_path_buf(),
                    stream_output: true,
                    output_bytes_cap: None,
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();

        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain first output");
        let first_events = parse_test_jsonl(&output);
        assert!(
            first_events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "first"
            }),
            "first delta should be emitted: {first_events:?}"
        );

        output.clear();
        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain second output");
        let second_events = parse_test_jsonl(&output);
        assert!(
            second_events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "later"
            }),
            "later delta should not be lost after retained output rebases: {second_events:?}"
        );
    }

    #[test]
    fn streaming_delta_respects_total_stdout_cap_across_reads() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-output-cap".to_string());
        let mut shell_sessions = RuntimeShellSessionManager::new(task_registry);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: platform_shell_script(
                    "printf ab; sleep 0.2; printf cd; sleep 0.2",
                    "[Console]::Out.Write('ab'); Start-Sleep -Milliseconds 200; [Console]::Out.Write('cd'); Start-Sleep -Milliseconds 200",
                ),
                argv: None,
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "stream output under cap".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-cap".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id),
                    command_event_id: Value::from("cmd-cap"),
                    command: platform_command_argv(),
                    cwd: cwd.path().to_path_buf(),
                    display_cwd: cwd.path().to_path_buf(),
                    stream_output: true,
                    output_bytes_cap: Some(3),
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();

        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain first output");
        let first_events = parse_test_jsonl(&output);
        assert!(
            first_events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "ab"
                    && event["capReached"] == false
            }),
            "first delta should be under cap: {first_events:?}"
        );

        output.clear();
        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain second output");
        let second_events = parse_test_jsonl(&output);
        assert!(
            second_events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "c"
                    && event["capReached"] == true
            }),
            "second delta should stop at the total stdout cap: {second_events:?}"
        );
        assert!(
            second_events.iter().all(|event| event["delta"] != "cd"),
            "second delta must not treat the cap as per-read: {second_events:?}"
        );
    }

    #[test]
    fn streaming_delta_reports_omitted_prefix_after_retained_output_rebase() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-output-omitted".to_string());
        let output_store = TaskOutputStore::with_max_retained_bytes(4);
        let mut shell_sessions =
            RuntimeShellSessionManager::with_output_store(task_registry, output_store);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: platform_shell_script(
                    "printf abcdef; sleep 0.2",
                    "[Console]::Out.Write('abcdef'); Start-Sleep -Milliseconds 200",
                ),
                argv: None,
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "stream omitted prefix".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-omitted".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id),
                    command_event_id: Value::from("cmd-omitted"),
                    command: platform_command_argv(),
                    cwd: cwd.path().to_path_buf(),
                    display_cwd: cwd.path().to_path_buf(),
                    stream_output: true,
                    output_bytes_cap: None,
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();

        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain output");
        let events = parse_test_jsonl(&output);
        assert!(
            events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "[2 bytes of earlier output omitted]\ncdef"
            }),
            "streaming delta should report omitted retained prefix: {events:?}"
        );
    }

    #[test]
    fn streaming_delta_reports_omitted_prefix_on_stderr_when_stdout_empty() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-stderr-omitted".to_string());
        let output_store = TaskOutputStore::with_max_retained_bytes(4);
        let mut shell_sessions =
            RuntimeShellSessionManager::with_output_store(task_registry, output_store);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: platform_shell_script(
                    "printf abcdef >&2; sleep 0.2",
                    "[Console]::Error.Write('abcdef'); Start-Sleep -Milliseconds 200",
                ),
                argv: None,
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "stream stderr omitted prefix".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-stderr-omitted".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id),
                    command_event_id: Value::from("cmd-stderr-omitted"),
                    command: platform_command_argv(),
                    cwd: cwd.path().to_path_buf(),
                    display_cwd: cwd.path().to_path_buf(),
                    stream_output: true,
                    output_bytes_cap: None,
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();

        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain output");
        let events = parse_test_jsonl(&output);
        assert!(
            events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stderr"
                    && event["delta"] == "[2 bytes of earlier output omitted]\ncdef"
            }),
            "stderr streaming delta should report omitted retained prefix: {events:?}"
        );
        assert!(
            events.iter().all(|event| event["stream"] != "stdout"),
            "pure stderr omission should not emit a stdout notice: {events:?}"
        );
    }

    #[test]
    fn streaming_delta_counts_omitted_output_toward_stdout_cap() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-omitted-cap".to_string());
        let output_store = TaskOutputStore::with_max_retained_bytes(4);
        let mut shell_sessions =
            RuntimeShellSessionManager::with_output_store(task_registry, output_store);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: platform_shell_script(
                    "printf abcdef; sleep 0.2",
                    "[Console]::Out.Write('abcdef'); Start-Sleep -Milliseconds 200",
                ),
                argv: None,
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "stream omitted output under cap".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-omitted-cap".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id),
                    command_event_id: Value::from("cmd-omitted-cap"),
                    command: platform_command_argv(),
                    cwd: cwd.path().to_path_buf(),
                    display_cwd: cwd.path().to_path_buf(),
                    stream_output: true,
                    output_bytes_cap: Some(3),
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();

        manager
            .drain_until_output_or_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain output");
        let events = parse_test_jsonl(&output);
        assert!(
            events.iter().any(|event| {
                event["event"] == "command_exec_output_delta"
                    && event["stream"] == "stdout"
                    && event["delta"] == "[2 bytes of earlier output omitted]\nc"
                    && event["capReached"] == true
            }),
            "omitted original stdout bytes should count toward stream cap: {events:?}"
        );
        assert!(
            events.iter().all(|event| event["delta"] != "cde"),
            "streaming cap must not restart from the retained tail: {events:?}"
        );
    }

    #[test]
    fn command_exec_denial_drain_evicts_finished_process_output() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-denial-evict".to_string());
        let mut shell_sessions = RuntimeShellSessionManager::new(task_registry);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: platform_shell_script("printf denied", "[Console]::Out.Write('denied')"),
                argv: None,
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "deny after output".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let (sender, receiver) = mpsc::channel();
        sender
            .send(RuntimeNetworkBlockReport {
                host: "blocked.orca.invalid".to_string(),
                error: "blocked-by-denylist",
            })
            .expect("send network block");
        drop(sender);

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-denied".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id.clone()),
                    command_event_id: Value::from("cmd-denied"),
                    command: platform_command_argv(),
                    cwd: cwd.path().to_path_buf(),
                    display_cwd: cwd.path().to_path_buf(),
                    stream_output: false,
                    output_bytes_cap: None,
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: Some(receiver),
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");
        let mut output = Vec::new();

        let outcome = manager
            .drain_with_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain denied process");

        assert!(matches!(
            outcome,
            super::CommandExecDrainOutcome::NetworkPermissionDenied { .. }
        ));
        assert_eq!(shell_sessions.output_store().size(&handle.task_id), 0);
    }

    #[test]
    fn command_exec_completion_survives_shell_snapshot_listing() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let task_registry = TaskRegistry::new("command-exec-list-ownership".to_string());
        let mut shell_sessions = RuntimeShellSessionManager::new(task_registry);
        let handle = shell_sessions
            .spawn(ShellSessionCommand {
                command: platform_shell_script(
                    "printf listed-complete",
                    "[Console]::Out.Write('listed-complete')",
                ),
                argv: None,
                cwd: cwd.path().to_path_buf(),
                additional_readable_directories: Vec::new(),
                additional_working_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: std::collections::BTreeMap::new(),
                description: "command exec survives shell list".to_string(),
                terminal: ShellTerminalMode::pipe(),
                sandbox: ShellSandboxMode::DangerFullAccess,
            })
            .expect("spawn shell");

        let mut manager = CommandExecManager::default();
        manager
            .insert(
                "proc-list-owned".to_string(),
                CommandExecProcess {
                    shell_id: Some(handle.id.clone()),
                    command_event_id: Value::from("cmd-list-owned"),
                    command: platform_command_argv(),
                    cwd: cwd.path().to_path_buf(),
                    display_cwd: cwd.path().to_path_buf(),
                    stream_output: false,
                    output_bytes_cap: None,
                    output_offset: 0,
                    stdout_len: 0,
                    stderr_len: 0,
                    stdout_cap_reached: false,
                    stderr_cap_reached: false,
                    network_permission_blocks: None,
                    permission_request: None,
                    _network_proxy: None,
                },
            )
            .expect("insert command exec process");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let terminal_status = loop {
            let status = shell_sessions
                .list()
                .into_iter()
                .find(|snapshot| snapshot.id == handle.id)
                .map(|snapshot| snapshot.status);
            if status.is_some_and(|status| {
                matches!(
                    status,
                    orca_core::task_types::TaskStatus::Stopped
                        | orca_core::task_types::TaskStatus::Completed
                        | orca_core::task_types::TaskStatus::Failed
                        | orca_core::task_types::TaskStatus::Cancelled
                )
            }) || std::time::Instant::now() >= deadline
            {
                break status;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            terminal_status.is_some_and(|status| {
                matches!(
                    status,
                    orca_core::task_types::TaskStatus::Stopped
                        | orca_core::task_types::TaskStatus::Completed
                        | orca_core::task_types::TaskStatus::Failed
                        | orca_core::task_types::TaskStatus::Cancelled
                )
            }),
            "command exec shell did not reach terminal state"
        );
        let _shell_snapshots = shell_sessions.list();

        let mut output = Vec::new();
        manager
            .drain_with_timeout(
                Some(&mut shell_sessions),
                &mut output,
                Duration::from_secs(1),
            )
            .expect("drain listed command exec process");
        let events = parse_test_jsonl(&output);

        assert!(
            events.iter().any(|event| {
                event["event"] == "command_exec_completed"
                    && event["processId"] == "proc-list-owned"
                    && event["stdout"] == "listed-complete"
            }),
            "command/exec completion should survive generic shell listing: {events:?}"
        );
        assert!(
            manager.list().is_empty(),
            "completed command/exec process should be removed after drain"
        );
    }

    fn parse_test_jsonl(stdout: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(stdout)
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
            .collect()
    }
}
