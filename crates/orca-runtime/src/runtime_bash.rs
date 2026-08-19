use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use orca_core::cancel::CancelToken;
use orca_core::config::{PermissionProfileNetworkAccess, RunConfig};
use orca_core::task_types::TaskStatus;
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult};

use crate::lifecycle::{RuntimePermissionRequestHandler, TurnPermissionOverlay};
use crate::network_proxy::{
    RuntimeNetworkBlockReport, RuntimeNetworkPolicy, RuntimeNetworkProxy,
    runtime_network_block_channel,
};
use crate::protocol::PermissionResponseDecision;
use crate::runtime_permission::{
    RuntimePermissionDecision, RuntimePermissionOrigin, RuntimePermissionPolicy,
    RuntimePermissionRequest, RuntimePermissionRequestKind,
};
use crate::runtime_state::PermissionRuntimeState;
use crate::sandbox_denial::{
    SandboxDenialDiagnostic, diagnose_sandbox_denial,
    should_request_filesystem_permission_with_denied_roots,
};
use crate::shell_session::{
    RuntimeShellSessionManager, ShellSandboxMode, ShellSessionCommand, ShellSessionTermination,
    ShellTerminalMode,
};
use crate::tasks::TaskRegistry;

pub(crate) struct RuntimeBashInvocationContext<'a, 'output> {
    pub(crate) config: Option<&'a RunConfig>,
    pub(crate) request: &'a ToolRequest,
    pub(crate) cwd: &'a Path,
    pub(crate) additional_roots: &'a [PathBuf],
    pub(crate) output_truncation: ToolOutputTruncation,
    pub(crate) shell_timeout_secs: u64,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) cancel: Option<&'a CancelToken>,
    pub(crate) permission_handler: Option<&'a dyn RuntimePermissionRequestHandler>,
    pub(crate) permission_overlay: &'a mut TurnPermissionOverlay,
    pub(crate) output_handler: Option<&'output mut dyn FnMut(&str)>,
}

pub(crate) fn execute_bash_with_shell_session(
    context: RuntimeBashInvocationContext<'_, '_>,
) -> ToolResult {
    let RuntimeBashInvocationContext {
        config,
        request,
        cwd,
        additional_roots,
        output_truncation,
        shell_timeout_secs,
        task_registry,
        cancel,
        permission_handler,
        permission_overlay,
        mut output_handler,
    } = context;
    let Some(command) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return ToolResult::failed(request, "bash command is required", None);
    };

    let Some(config) = config else {
        return execute_bash_once(RuntimeBashOnceContext {
            command,
            cwd,
            additional_readable_directories: Vec::new(),
            additional_working_directories: additional_roots.to_vec(),
            metadata_writable_directories: Vec::new(),
            denied_working_directories: Vec::new(),
            allowed_unix_socket_roots: Vec::new(),
            env: Default::default(),
            sandbox: ShellSandboxMode::default(),
            shell_timeout_secs,
            task_registry,
            cancel,
            output_handler: reborrow_output_handler(&mut output_handler),
        })
        .into_tool_result(request, output_truncation, shell_timeout_secs);
    };
    let mut sandbox = match bash_sandbox_from_active_permission_profile(config, cwd) {
        Ok(sandbox) => sandbox,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    for (domain, access) in permission_overlay.network_domain_permissions() {
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
    for root in permission_overlay.additional_working_directories() {
        push_unique_path(&mut sandbox.additional_writable_roots, root.clone());
    }
    for root in permission_overlay.metadata_writable_directories() {
        push_unique_path(&mut sandbox.metadata_writable_roots, root.clone());
    }
    let result = execute_bash_with_sandbox(RuntimeBashSandboxContext {
        command,
        cwd,
        additional_roots,
        sandbox: &sandbox,
        shell_timeout_secs,
        task_registry,
        cancel,
        output_handler: reborrow_output_handler(&mut output_handler),
    });
    let BashExecutionResult {
        output,
        network_block,
    } = result;
    if let Some(block) = network_block
        && let Some(permission_prompt) =
            RuntimeBashPermissionPolicy::network_block_prompt(&request.id, &block)
        && let Some(permission_handler) = permission_handler
    {
        let (_origin, _kind, permission_request) = permission_prompt.into_request_parts();
        let response = match PermissionRuntimeState.request_permission_pre_side_effect(
            permission_overlay,
            permission_handler,
            permission_request,
        ) {
            Ok(response) => response,
            Err(error) => return ToolResult::failed(request, error.to_string(), None),
        };
        if response.decision == PermissionResponseDecision::Deny {
            return ToolResult::denied(request, "permission request denied".to_string());
        }
        let mut retry_sandbox = sandbox;
        if let Some(network) = response.permissions.network {
            for (domain, access) in network.domains {
                retry_sandbox.network_policy_domains.insert(domain, access);
            }
        }
        return execute_bash_with_sandbox(RuntimeBashSandboxContext {
            command,
            cwd,
            additional_roots,
            sandbox: &retry_sandbox,
            shell_timeout_secs,
            task_registry,
            cancel,
            output_handler: reborrow_output_handler(&mut output_handler),
        })
        .output
        .into_tool_result(request, output_truncation, shell_timeout_secs);
    }
    if let Some(diagnostic) = output.sandbox_denial_diagnostic(cwd) {
        if should_request_filesystem_permission_with_denied_roots(
            cwd,
            &diagnostic,
            &sandbox.denied_writable_roots,
        ) && let Some(permission_prompt) =
            RuntimeBashPermissionPolicy::filesystem_write_prompt(&request.id, &diagnostic)
            && let Some(permission_handler) = permission_handler
        {
            let (_origin, _kind, permission_request) = permission_prompt.into_request_parts();
            let response = match PermissionRuntimeState.request_permission_pre_side_effect(
                permission_overlay,
                permission_handler,
                permission_request,
            ) {
                Ok(response) => response,
                Err(error) => return ToolResult::failed(request, error.to_string(), None),
            };
            if response.decision == PermissionResponseDecision::Deny {
                return ToolResult::denied(request, "permission request denied".to_string());
            }

            let mut retry_sandbox = sandbox;
            for root in permission_overlay.additional_working_directories() {
                push_unique_path(&mut retry_sandbox.additional_writable_roots, root.clone());
            }
            for root in permission_overlay.metadata_writable_directories() {
                push_unique_path(&mut retry_sandbox.metadata_writable_roots, root.clone());
            }

            return execute_bash_with_sandbox(RuntimeBashSandboxContext {
                command,
                cwd,
                additional_roots,
                sandbox: &retry_sandbox,
                shell_timeout_secs,
                task_registry,
                cancel,
                output_handler: reborrow_output_handler(&mut output_handler),
            })
            .output
            .with_sandbox_diagnostic(cwd)
            .into_tool_result(request, output_truncation, shell_timeout_secs);
        }
        if let Some(permission_prompt) =
            RuntimeBashPermissionPolicy::unsandboxed_shell_prompt(&request.id, &diagnostic)
            && let Some(permission_handler) = permission_handler
        {
            let (_origin, _kind, permission_request) = permission_prompt.into_request_parts();
            let response = match PermissionRuntimeState.request_permission_pre_side_effect(
                permission_overlay,
                permission_handler,
                permission_request,
            ) {
                Ok(response) => response,
                Err(error) => return ToolResult::failed(request, error.to_string(), None),
            };
            if response.decision == PermissionResponseDecision::Deny {
                return ToolResult::denied(request, "permission request denied".to_string());
            }

            return execute_bash_once(RuntimeBashOnceContext {
                command,
                cwd,
                additional_readable_directories: Vec::new(),
                additional_working_directories: additional_roots.to_vec(),
                metadata_writable_directories: Vec::new(),
                denied_working_directories: Vec::new(),
                allowed_unix_socket_roots: Vec::new(),
                env: Default::default(),
                sandbox: ShellSandboxMode::DangerFullAccess,
                shell_timeout_secs,
                task_registry,
                cancel,
                output_handler: reborrow_output_handler(&mut output_handler),
            })
            .with_sandbox_diagnostic(cwd)
            .into_tool_result(request, output_truncation, shell_timeout_secs);
        }
        return output.with_diagnostic(diagnostic).into_tool_result(
            request,
            output_truncation,
            shell_timeout_secs,
        );
    }
    output.into_tool_result(request, output_truncation, shell_timeout_secs)
}

fn bash_sandbox_from_active_permission_profile(
    config: &RunConfig,
    cwd: &Path,
) -> Result<crate::server::CommandExecSandbox, String> {
    crate::server::bash_sandbox_for_cwd(config, cwd)
}

struct BashExecutionResult {
    output: BashShellOutput,
    network_block: Option<RuntimeNetworkBlockReport>,
}

struct BashShellOutput {
    output: Result<crate::shell_session::ShellSessionOutput, String>,
}

struct RuntimeBashSandboxContext<'a, 'output> {
    command: &'a str,
    cwd: &'a Path,
    additional_roots: &'a [PathBuf],
    sandbox: &'a crate::server::CommandExecSandbox,
    shell_timeout_secs: u64,
    task_registry: &'a TaskRegistry,
    cancel: Option<&'a CancelToken>,
    output_handler: Option<&'output mut dyn FnMut(&str)>,
}

struct RuntimeBashOnceContext<'a, 'output> {
    command: &'a str,
    cwd: &'a Path,
    additional_readable_directories: Vec<PathBuf>,
    additional_working_directories: Vec<PathBuf>,
    metadata_writable_directories: Vec<PathBuf>,
    denied_working_directories: Vec<PathBuf>,
    allowed_unix_socket_roots: Vec<PathBuf>,
    env: BTreeMap<String, Option<String>>,
    sandbox: ShellSandboxMode,
    shell_timeout_secs: u64,
    task_registry: &'a TaskRegistry,
    cancel: Option<&'a CancelToken>,
    output_handler: Option<&'output mut dyn FnMut(&str)>,
}

struct RuntimeBashPermissionPrompt {
    origin: RuntimePermissionOrigin,
    kind: RuntimePermissionRequestKind,
    request: RuntimePermissionRequest,
}

impl From<RuntimePermissionDecision> for RuntimeBashPermissionPrompt {
    fn from(decision: RuntimePermissionDecision) -> Self {
        Self {
            origin: decision.origin,
            kind: decision.kind,
            request: decision.request,
        }
    }
}

impl RuntimeBashPermissionPrompt {
    fn into_request_parts(
        self,
    ) -> (
        RuntimePermissionOrigin,
        RuntimePermissionRequestKind,
        RuntimePermissionRequest,
    ) {
        (self.origin, self.kind, self.request)
    }
}

struct RuntimeBashPermissionPolicy;

impl RuntimeBashPermissionPolicy {
    fn network_block_prompt(
        request_id: &str,
        block: &RuntimeNetworkBlockReport,
    ) -> Option<RuntimeBashPermissionPrompt> {
        RuntimePermissionPolicy::network_block_decision(
            request_id,
            RuntimePermissionOrigin::Bash,
            block,
        )
        .map(RuntimeBashPermissionPrompt::from)
    }

    fn filesystem_write_prompt(
        request_id: &str,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> Option<RuntimeBashPermissionPrompt> {
        RuntimePermissionPolicy::filesystem_write_decision(
            request_id,
            RuntimePermissionOrigin::Bash,
            diagnostic,
        )
        .map(RuntimeBashPermissionPrompt::from)
    }

    fn unsandboxed_shell_prompt(
        request_id: &str,
        diagnostic: &SandboxDenialDiagnostic,
    ) -> Option<RuntimeBashPermissionPrompt> {
        RuntimePermissionPolicy::unsandboxed_shell_decision(
            request_id,
            RuntimePermissionOrigin::Bash,
            diagnostic,
        )
        .map(RuntimeBashPermissionPrompt::from)
    }
}

impl BashShellOutput {
    fn sandbox_denial_diagnostic(&self, cwd: &Path) -> Option<SandboxDenialDiagnostic> {
        let output = self.output.as_ref().ok()?;
        if output.status == TaskStatus::Completed {
            return None;
        }
        diagnose_sandbox_denial(cwd, &output.stdout, &output.stderr)
    }

    fn with_sandbox_diagnostic(self, cwd: &Path) -> Self {
        let Some(diagnostic) = self.sandbox_denial_diagnostic(cwd) else {
            return self;
        };
        self.with_diagnostic(diagnostic)
    }

    fn with_diagnostic(mut self, diagnostic: SandboxDenialDiagnostic) -> Self {
        if let Ok(output) = &mut self.output {
            if output.stderr.trim_end().is_empty() {
                output.stderr = diagnostic.message;
            } else {
                output
                    .stderr
                    .push_str(&format!("\n\nSandbox diagnostic: {}", diagnostic.message));
            }
        }
        self
    }

    fn into_tool_result(
        self,
        request: &ToolRequest,
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
    ) -> ToolResult {
        let output = match self.output {
            Ok(output) => output,
            Err(error) => return ToolResult::failed(request, error, None),
        };
        let stdout = output.stdout.trim_end().to_string();
        let stderr = output.stderr.trim_end().to_string();
        if output.termination == ShellSessionTermination::Cancelled {
            let message = if stderr.is_empty() && stdout.is_empty() {
                "shell command cancelled".to_string()
            } else if stderr.is_empty() {
                format!("shell command cancelled: {stdout}")
            } else if stdout.is_empty() {
                format!("shell command cancelled: {stderr}")
            } else {
                format!("shell command cancelled: {stdout}\n{stderr}")
            };
            let (message, truncated) =
                orca_core::tool_types::truncate_output_with_policy(message, output_truncation);
            let mut result = ToolResult::cancelled(request, message, output.exit_code);
            result.set_truncated(truncated);
            return result;
        }
        if output.termination == ShellSessionTermination::TimedOut {
            let timeout_secs = shell_timeout_secs.max(1);
            let message = if stderr.is_empty() && stdout.is_empty() {
                format!("shell command timed out after {timeout_secs}s")
            } else if stderr.is_empty() {
                format!("shell command timed out after {timeout_secs}s: {stdout}")
            } else if stdout.is_empty() {
                format!("shell command timed out after {timeout_secs}s: {stderr}")
            } else {
                format!("shell command timed out after {timeout_secs}s: {stdout}\n{stderr}")
            };
            let (message, truncated) =
                orca_core::tool_types::truncate_output_with_policy(message, output_truncation);
            let mut result = ToolResult::failed(request, message, output.exit_code);
            result.set_truncated(truncated);
            return result;
        }
        if output.status == TaskStatus::Completed {
            let (stdout, truncated) =
                orca_core::tool_types::truncate_output_with_policy(stdout, output_truncation);
            return ToolResult::completed(request, stdout, truncated);
        }

        let message = if stderr.is_empty() {
            stdout
        } else if stdout.is_empty() {
            stderr
        } else {
            format!("{stdout}\n{stderr}")
        };
        let (message, truncated) =
            orca_core::tool_types::truncate_output_with_policy(message, output_truncation);
        let mut result = ToolResult::failed(request, message, output.exit_code);
        result.set_truncated(truncated);
        result
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn execute_bash_with_sandbox(context: RuntimeBashSandboxContext<'_, '_>) -> BashExecutionResult {
    let RuntimeBashSandboxContext {
        command,
        cwd,
        additional_roots,
        sandbox,
        shell_timeout_secs,
        task_registry,
        cancel,
        output_handler,
    } = context;
    let mut additional_working_directories = additional_roots.to_vec();
    additional_working_directories.extend(sandbox.additional_writable_roots.clone());
    #[cfg(windows)]
    if !sandbox.network_policy_domains.is_empty() {
        return BashExecutionResult {
            output: BashShellOutput {
                output: Err(
                    "Windows domain-restricted network sandbox is unavailable; refusing to run without an OS-enforced network boundary".to_string(),
                ),
            },
            network_block: None,
        };
    }
    let mut env = BTreeMap::new();
    let mut block_receiver = None;
    let _network_proxy = if sandbox.network_policy_domains.is_empty() {
        None
    } else {
        let (sender, receiver) = runtime_network_block_channel();
        block_receiver = Some(receiver);
        match RuntimeNetworkProxy::start_with_block_reporter(
            RuntimeNetworkPolicy::new(sandbox.network_policy_domains.clone()),
            Some(sender),
        ) {
            Ok(proxy) => {
                for key in [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                ] {
                    env.insert(key.to_string(), Some(proxy.proxy_url().to_string()));
                }
                for key in ["NO_PROXY", "no_proxy"] {
                    env.insert(key.to_string(), None);
                }
                Some(proxy)
            }
            Err(error) => {
                return BashExecutionResult {
                    output: BashShellOutput {
                        output: Err(format!("failed to start network proxy: {error}")),
                    },
                    network_block: None,
                };
            }
        }
    };
    let output = execute_bash_once(RuntimeBashOnceContext {
        command,
        cwd,
        additional_readable_directories: sandbox.additional_readable_roots.clone(),
        additional_working_directories,
        metadata_writable_directories: sandbox.metadata_writable_roots.clone(),
        denied_working_directories: sandbox.denied_writable_roots.clone(),
        allowed_unix_socket_roots: sandbox.allowed_unix_socket_roots.clone(),
        env,
        sandbox: sandbox.mode,
        shell_timeout_secs,
        task_registry,
        cancel,
        output_handler,
    });
    let network_block = block_receiver.and_then(|receiver| {
        receiver
            .try_iter()
            .find(|block| block.error != "blocked-by-denylist")
    });
    BashExecutionResult {
        output,
        network_block,
    }
}

fn execute_bash_once(context: RuntimeBashOnceContext<'_, '_>) -> BashShellOutput {
    let RuntimeBashOnceContext {
        command,
        cwd,
        additional_readable_directories,
        additional_working_directories,
        metadata_writable_directories,
        denied_working_directories,
        allowed_unix_socket_roots,
        env,
        sandbox,
        shell_timeout_secs,
        task_registry,
        cancel,
        output_handler,
    } = context;
    let mut manager = RuntimeShellSessionManager::new(task_registry.clone());
    let handle = match manager.spawn_with_metadata_roots(
        ShellSessionCommand {
            command: command.to_string(),
            argv: None,
            cwd: cwd.to_path_buf(),
            additional_readable_directories,
            additional_working_directories,
            denied_working_directories,
            allowed_unix_socket_roots,
            env,
            description: command.to_string(),
            terminal: ShellTerminalMode::pipe(),
            sandbox,
        },
        metadata_writable_directories,
    ) {
        Ok(handle) => handle,
        Err(error) => {
            return BashShellOutput {
                output: Err(format!("failed to run shell command: {error}")),
            };
        }
    };
    let _ = manager.close_stdin(&handle.id);
    fn discard_output(_: &str) {}
    let mut discard_output = discard_output;
    let output_handler = output_handler.unwrap_or(&mut discard_output);
    let output = match manager.wait_or_cancel_with_output(
        &handle.id,
        std::time::Duration::from_secs(shell_timeout_secs.max(1)),
        || {
            cancel.is_some_and(CancelToken::is_cancelled)
                || task_registry.is_cancelled(&handle.task_id)
        },
        output_handler,
    ) {
        Ok(output) => Ok(output),
        Err(error) => Err(format!("failed to wait for shell command: {error}")),
    };
    BashShellOutput { output }
}

fn reborrow_output_handler<'borrow, 'handler>(
    handler: &'borrow mut Option<&'handler mut dyn FnMut(&str)>,
) -> Option<&'borrow mut dyn FnMut(&str)> {
    match handler {
        Some(handler) => Some(&mut **handler),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::network_proxy::RuntimeNetworkBlockReport;
    use crate::runtime_permission::{RuntimePermissionOrigin, RuntimePermissionRequestKind};
    use crate::sandbox_denial::SandboxDenialDiagnostic;

    use super::RuntimeBashPermissionPolicy;

    #[test]
    fn runtime_bash_permission_policy_preserves_network_decision_metadata() {
        let block = RuntimeNetworkBlockReport {
            host: "api.orca.invalid".to_string(),
            error: "blocked-by-allowlist",
        };

        let prompt =
            RuntimeBashPermissionPolicy::network_block_prompt("bash-1", &block).expect("prompt");

        assert_eq!(prompt.origin, RuntimePermissionOrigin::Bash);
        assert_eq!(prompt.kind, RuntimePermissionRequestKind::NetworkBlock);
        assert_eq!(
            prompt.request.reason.as_deref(),
            Some("bash attempted network access to api.orca.invalid (blocked-by-allowlist)")
        );
    }

    #[test]
    fn runtime_bash_permission_policy_preserves_filesystem_decision_metadata() {
        let diagnostic = SandboxDenialDiagnostic {
            denied_path: Some(PathBuf::from("/repo/.git/index.lock")),
            suggested_write_root: Some(PathBuf::from("/repo/.git")),
            message: "sandbox denied filesystem access".to_string(),
        };

        let prompt = RuntimeBashPermissionPolicy::filesystem_write_prompt("bash-1", &diagnostic)
            .expect("prompt");

        assert_eq!(prompt.origin, RuntimePermissionOrigin::Bash);
        assert_eq!(prompt.kind, RuntimePermissionRequestKind::FilesystemWrite);
        assert_eq!(
            prompt
                .request
                .permissions
                .file_system
                .as_ref()
                .and_then(|file_system| file_system.write.as_ref()),
            Some(&vec![PathBuf::from("/repo/.git")])
        );
    }

    #[test]
    fn runtime_bash_permission_policy_preserves_unsandboxed_decision_metadata() {
        let diagnostic = SandboxDenialDiagnostic {
            denied_path: None,
            suggested_write_root: None,
            message: "sandbox denied filesystem access".to_string(),
        };

        let prompt = RuntimeBashPermissionPolicy::unsandboxed_shell_prompt("bash-1", &diagnostic)
            .expect("prompt");

        assert_eq!(prompt.origin, RuntimePermissionOrigin::Bash);
        assert_eq!(
            prompt.kind,
            RuntimePermissionRequestKind::UnsandboxedShellRetry
        );
        assert!(
            prompt
                .request
                .permissions
                .shell
                .as_ref()
                .is_some_and(|shell| shell.unsandboxed)
        );
    }
}
