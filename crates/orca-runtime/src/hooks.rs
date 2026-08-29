use std::fmt;
use std::process::Stdio;
use std::time::Duration;

use orca_core::cancel::CancelToken;
use orca_core::capability::CapabilitySet;
use orca_core::conversation::Conversation;
use orca_core::execution_broker::{ExecutionBroker, LaunchError};
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::provider_types::Usage;
use orca_core::tool_types::{ToolRequest, ToolResult};

#[derive(Clone, Debug, Default)]
pub struct HookRunner {
    hooks: Vec<HookConfig>,
    capabilities: CapabilitySet,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HookOutcome {
    pub modified_target: Option<String>,
    pub injected_context: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HookRunError {
    Cancelled(String),
    Failed(String),
}

impl fmt::Display for HookRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled(message) | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for HookRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct HookContext<'a> {
    pub cwd: &'a str,
    pub session_status: Option<&'a str>,
    pub tool_request: Option<&'a ToolRequest>,
    pub tool_result: Option<&'a ToolResult>,
    pub before_messages: Option<usize>,
    pub after_messages: Option<usize>,
    pub usage: Option<&'a Usage>,
}

impl HookRunner {
    pub fn new(hooks: Vec<HookConfig>) -> Self {
        Self {
            hooks,
            capabilities: CapabilitySet::read_only(),
        }
    }

    pub fn new_with_capabilities(hooks: Vec<HookConfig>, capabilities: CapabilitySet) -> Self {
        Self {
            hooks,
            capabilities,
        }
    }

    pub fn run(&self, event: HookEvent, context: HookContext<'_>) -> Result<HookOutcome, String> {
        self.run_with_timeout(event, context, Duration::from_secs(30))
    }

    pub fn run_with_cancel(
        &self,
        event: HookEvent,
        context: HookContext<'_>,
        cancel: &CancelToken,
    ) -> Result<HookOutcome, String> {
        self.run_with_cancel_result(event, context, cancel)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn run_with_cancel_result(
        &self,
        event: HookEvent,
        context: HookContext<'_>,
        cancel: &CancelToken,
    ) -> Result<HookOutcome, HookRunError> {
        self.run_with_timeout_or_cancel_result(event, context, Duration::from_secs(30), || {
            cancel.is_cancelled()
        })
    }

    fn run_with_timeout(
        &self,
        event: HookEvent,
        context: HookContext<'_>,
        timeout: Duration,
    ) -> Result<HookOutcome, String> {
        self.run_with_timeout_or_cancel(event, context, timeout, || false)
    }

    fn run_with_timeout_or_cancel(
        &self,
        event: HookEvent,
        context: HookContext<'_>,
        timeout: Duration,
        should_cancel: impl Fn() -> bool,
    ) -> Result<HookOutcome, String> {
        self.run_with_timeout_or_cancel_result(event, context, timeout, should_cancel)
            .map_err(|error| error.to_string())
    }

    fn run_with_timeout_or_cancel_result(
        &self,
        event: HookEvent,
        context: HookContext<'_>,
        timeout: Duration,
        should_cancel: impl Fn() -> bool,
    ) -> Result<HookOutcome, HookRunError> {
        let mut outcome = HookOutcome::default();
        for hook in self.matching_hooks(event, context.tool_request) {
            let shell = orca_platform::shell::ShellResolver::for_current_host()
                .resolve_from_environment()
                .map_err(|error| {
                    HookRunError::Failed(format!(
                        "hook '{}' could not resolve the host shell: {error}",
                        hook.command
                    ))
                })?;
            let mut command = orca_tools::process::shell_command(&shell, &hook.command);
            command
                .env("ORCA_HOOK_EVENT", event.as_str())
                .env("ORCA_CWD", context.cwd)
                .env(
                    "ORCA_SESSION_STATUS",
                    context.session_status.unwrap_or_default(),
                )
                .env(
                    "ORCA_TOOL_NAME",
                    context
                        .tool_request
                        .map(|request| request.name.as_str())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_TOOL_TARGET",
                    sanitize_env_value(
                        context
                            .tool_request
                            .and_then(|request| request.target.as_deref())
                            .unwrap_or_default(),
                    ),
                )
                .env(
                    "ORCA_TOOL_STATUS",
                    context
                        .tool_result
                        .map(|result| result.status.as_str())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_COMPACT_BEFORE_MESSAGES",
                    context
                        .before_messages
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_COMPACT_AFTER_MESSAGES",
                    context
                        .after_messages
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_USAGE_INPUT_TOKENS",
                    context
                        .usage
                        .map(|usage| usage.input_tokens.to_string())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_USAGE_OUTPUT_TOKENS",
                    context
                        .usage
                        .map(|usage| usage.output_tokens.to_string())
                        .unwrap_or_default(),
                )
                .env(
                    "ORCA_USAGE_CACHE_TOKENS",
                    context
                        .usage
                        .map(|usage| usage.cache_tokens.to_string())
                        .unwrap_or_default(),
                )
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            orca_tools::process::prepare_non_interactive_command(&mut command);
            let broker = ExecutionBroker::with_backend(
                orca_core::capability::EnforcementState::Advisory,
                "hook-user-trusted",
            );
            let (child, process_job) = broker
                .launch_user_trusted(
                    command,
                    format!("hook:{:?}", event),
                    context.cwd,
                    self.capabilities.clone(),
                )
                .map(|launched| (launched.child, launched.process_job))
                .map_err(|error| {
                    let detail = match error {
                        LaunchError::Spawn(error) => error.to_string(),
                        other => format!("{other:?}"),
                    };
                    HookRunError::Failed(format!(
                        "hook '{}' failed to start: {detail}",
                        hook.command
                    ))
                })?;
            let output = orca_tools::process::wait_for_child_output_with_timeout_or_cancel(
                child,
                process_job,
                timeout,
                &should_cancel,
            )
            .map_err(|error| {
                HookRunError::Failed(format!("hook '{}' failed: {error}", hook.command))
            })?;
            if output.termination == orca_tools::process::CommandTermination::Cancelled {
                return Err(HookRunError::Cancelled(format!(
                    "hook '{}' cancelled",
                    hook.command
                )));
            }

            if output.timed_out {
                let stderr =
                    String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(65536)])
                        .trim()
                        .to_string();
                let stdout =
                    String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(65536)])
                        .trim()
                        .to_string();
                let detail = match (stdout.is_empty(), stderr.is_empty()) {
                    (true, true) => String::new(),
                    (false, true) => stdout,
                    (true, false) => stderr,
                    (false, false) => format!("{stdout}\n{stderr}"),
                };
                return Err(HookRunError::Failed(if detail.is_empty() {
                    format!(
                        "hook '{}' timed out after {}s",
                        hook.command,
                        timeout.as_secs()
                    )
                } else {
                    format!(
                        "hook '{}' timed out after {}s: {detail}",
                        hook.command,
                        timeout.as_secs()
                    )
                }));
            }

            if !output.status.success() {
                let stderr =
                    String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(65536)])
                        .trim()
                        .to_string();
                let stdout =
                    String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(65536)])
                        .trim()
                        .to_string();
                let detail = if stderr.is_empty() { stdout } else { stderr };
                return Err(HookRunError::Failed(if detail.is_empty() {
                    format!("hook '{}' exited with {}", hook.command, output.status)
                } else {
                    format!(
                        "hook '{}' exited with {}: {detail}",
                        hook.command, output.status
                    )
                }));
            }

            let stdout = String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(65536)])
                .trim()
                .to_string();
            apply_hook_stdout(&stdout, &mut outcome).map_err(HookRunError::Failed)?;
        }

        Ok(outcome)
    }

    fn matching_hooks<'a>(
        &'a self,
        event: HookEvent,
        tool_request: Option<&ToolRequest>,
    ) -> impl Iterator<Item = &'a HookConfig> {
        self.hooks.iter().filter(move |hook| {
            hook.event == event
                && hook
                    .tool
                    .as_deref()
                    .map(|tool| {
                        tool_request
                            .map(|request| {
                                request.name.as_str() == tool
                                    || (request.name
                                        == orca_core::tool_types::ToolName::ExecCommand
                                        && tool == "bash")
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(true)
        })
    }
}

fn apply_hook_stdout(stdout: &str, outcome: &mut HookOutcome) -> Result<(), String> {
    if stdout.is_empty() {
        return Ok(());
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) else {
        outcome.injected_context.push(stdout.to_string());
        return Ok(());
    };

    let Some(action) = value.get("action").and_then(|value| value.as_str()) else {
        outcome.injected_context.push(stdout.to_string());
        return Ok(());
    };

    match action {
        "allow" => Ok(()),
        "deny" => Err(value
            .get("reason")
            .and_then(|value| value.as_str())
            .unwrap_or("hook denied request")
            .to_string()),
        "modify" => {
            let target = value
                .get("modified_target")
                .or_else(|| value.get("modified_input"))
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    "hook action 'modify' requires string field 'modified_target'".to_string()
                })?;
            outcome.modified_target = Some(target.to_string());
            Ok(())
        }
        "inject" => {
            let context = value
                .get("context")
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    "hook action 'inject' requires string field 'context'".to_string()
                })?;
            outcome.injected_context.push(context.to_string());
            Ok(())
        }
        _ => Err(format!("unsupported hook action '{action}'")),
    }
}

pub fn conversation_with_hook_context(
    conversation: &Conversation,
    outcome: &HookOutcome,
) -> Conversation {
    let mut conversation = conversation.clone();
    if !outcome.injected_context.is_empty() {
        conversation.add_system_pinned(format!(
            "[Hook context]\n{}",
            outcome.injected_context.join("\n\n")
        ));
    }
    conversation
}

pub fn tool_request_with_hook_outcome(request: &ToolRequest, outcome: &HookOutcome) -> ToolRequest {
    let mut request = request.clone();
    if let Some(target) = outcome.modified_target.as_ref() {
        request.target = Some(target.clone());
    }
    request
}

fn sanitize_env_value(value: &str) -> String {
    const MAX_ENV_VALUE_LEN: usize = 4096;
    let sanitized: String = value
        .chars()
        .take(MAX_ENV_VALUE_LEN)
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\0' {
                ' '
            } else {
                c
            }
        })
        .collect();
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::provider_types::Usage;
    use orca_core::tool_types::{ToolName, ToolRequest};
    use std::time::Instant;

    fn platform_hook_script(unix: &str, windows: &str) -> String {
        if cfg!(windows) {
            windows.to_string()
        } else {
            unix.to_string()
        }
    }

    fn platform_test_delay(unix_ms: u64, windows_ms: u64) -> Duration {
        Duration::from_millis(if cfg!(windows) { windows_ms } else { unix_ms })
    }

    fn hook_stdout_script(stdout: &str) -> String {
        platform_hook_script(
            &format!("printf '%s' '{}'", stdout.replace('\'', "'\\''")),
            &format!("[Console]::Out.Write('{}')", stdout.replace('\'', "''")),
        )
    }

    fn blocking_hook_script() -> String {
        platform_hook_script(
            "echo blocked >&2; exit 7",
            "[Console]::Error.Write('blocked'); exit 7",
        )
    }

    #[test]
    fn pre_tool_hook_can_block() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: blocking_hook_script(),
            tool: Some("bash".to_string()),
        }]);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let err = runner
            .run(
                HookEvent::PreToolUse,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: Some(&request),
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .unwrap_err();
        assert!(err.contains("blocked"));
    }

    #[test]
    fn bash_hooks_match_exec_command_for_compatibility() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: "true".to_string(),
            tool: Some("bash".to_string()),
        }]);
        let request = ToolRequest {
            id: "tool-exec".to_string(),
            name: ToolName::ExecCommand,
            action: ActionKind::Shell,
            target: Some("printf test".to_string()),
            raw_arguments: Some(r#"{"cmd":"printf test"}"#.to_string()),
        };

        assert_eq!(
            runner
                .matching_hooks(HookEvent::PreToolUse, Some(&request))
                .count(),
            1
        );
    }

    #[test]
    fn typed_hook_failure_is_not_relabelled_by_late_cancellation() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: blocking_hook_script(),
            tool: Some("bash".to_string()),
        }]);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let cancel = CancelToken::new();

        let error = runner
            .run_with_cancel_result(
                HookEvent::PreToolUse,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: Some(&request),
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
                &cancel,
            )
            .expect_err("hook should fail");
        cancel.cancel();

        assert!(matches!(error, HookRunError::Failed(message) if message.contains("blocked")));
    }

    #[test]
    fn hook_timeout_kills_descendant_processes() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreModelCall,
            command: platform_hook_script(
                "printf before; sleep 5; printf after",
                "[Console]::Out.Write('before'); [Console]::Out.Flush(); Start-Sleep -Seconds 5; [Console]::Out.Write('after')",
            ),
            tool: None,
        }]);
        let start = Instant::now();

        let err = runner
            .run_with_timeout(
                HookEvent::PreModelCall,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
                platform_test_delay(200, 1_500),
            )
            .unwrap_err();

        assert!(
            start.elapsed() < Duration::from_secs(4),
            "hook should not wait for descendant processes"
        );
        assert!(
            err.contains("timed out after") && err.contains("before"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parses_new_model_and_budget_hook_events() {
        assert_eq!(
            toml::from_str::<HookConfig>(
                r#"
event = "pre_model_call"
command = "true"
"#,
            )
            .unwrap()
            .event,
            HookEvent::PreModelCall
        );
        assert_eq!(
            toml::from_str::<HookConfig>(
                r#"
event = "post_model_call"
command = "true"
"#,
            )
            .unwrap()
            .event,
            HookEvent::PostModelCall
        );
        assert_eq!(
            toml::from_str::<HookConfig>(
                r#"
event = "on_budget_warning"
command = "true"
"#,
            )
            .unwrap()
            .event,
            HookEvent::OnBudgetWarning
        );
        assert_eq!(HookEvent::PreModelCall.as_str(), "pre_model_call");
        assert_eq!(HookEvent::PostModelCall.as_str(), "post_model_call");
        assert_eq!(HookEvent::OnBudgetWarning.as_str(), "on_budget_warning");
    }

    #[test]
    fn hook_json_deny_blocks_with_reason_even_when_exit_succeeds() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: hook_stdout_script(r#"{"action":"deny","reason":"violates policy X"}"#),
            tool: Some("bash".to_string()),
        }]);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo secret".to_string()),
            raw_arguments: None,
        };

        let err = runner
            .run(
                HookEvent::PreToolUse,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: Some(&request),
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .unwrap_err();

        assert_eq!(err, "violates policy X");
    }

    #[test]
    fn hook_json_modify_returns_modified_target() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: hook_stdout_script(
                r#"{"action":"modify","modified_target":"ls -la (sanitized)"}"#,
            ),
            tool: Some("bash".to_string()),
        }]);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("ls -la /tmp".to_string()),
            raw_arguments: None,
        };

        let outcome = runner
            .run(
                HookEvent::PreToolUse,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: Some(&request),
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .unwrap();

        assert_eq!(
            outcome.modified_target.as_deref(),
            Some("ls -la (sanitized)")
        );
    }

    #[test]
    fn hook_json_rejects_unknown_action_instead_of_injecting_it() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreModelCall,
            command: hook_stdout_script(r#"{"action":"injcet","context":"typo should fail"}"#),
            tool: None,
        }]);

        let err = runner
            .run(
                HookEvent::PreModelCall,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .expect_err("unknown structured hook actions should fail");

        assert!(
            err.contains("unsupported hook action 'injcet'"),
            "err={err}"
        );
    }

    #[test]
    fn hook_json_rejects_inject_without_string_context() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreModelCall,
            command: hook_stdout_script(r#"{"action":"inject","context":123}"#),
            tool: None,
        }]);

        let err = runner
            .run(
                HookEvent::PreModelCall,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .expect_err("inject action should require string context");

        assert!(
            err.contains("hook action 'inject' requires string field 'context'"),
            "err={err}"
        );
    }

    #[test]
    fn hook_json_rejects_modify_without_string_target() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreToolUse,
            command: hook_stdout_script(r#"{"action":"modify","modified_target":123}"#),
            tool: Some("bash".to_string()),
        }]);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo original".to_string()),
            raw_arguments: None,
        };

        let err = runner
            .run(
                HookEvent::PreToolUse,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: Some(&request),
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .expect_err("modify action should require a string target");

        assert!(
            err.contains("hook action 'modify' requires string field 'modified_target'"),
            "err={err}"
        );
    }

    #[test]
    fn hook_json_and_plain_stdout_can_inject_context() {
        let runner = HookRunner::new(vec![
            HookConfig {
                event: HookEvent::PreModelCall,
                command: hook_stdout_script(r#"{"action":"inject","context":"policy hint"}"#),
                tool: None,
            },
            HookConfig {
                event: HookEvent::PreModelCall,
                command: hook_stdout_script("legacy hint"),
                tool: None,
            },
        ]);

        let outcome = runner
            .run(
                HookEvent::PreModelCall,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            )
            .unwrap();

        assert_eq!(outcome.injected_context, vec!["policy hint", "legacy hint"]);
    }

    #[test]
    fn post_model_call_hook_receives_usage_environment() {
        let runner = HookRunner::new(vec![HookConfig {
            event: HookEvent::PostModelCall,
            command: platform_hook_script(
                concat!(
                    r#"test "$ORCA_USAGE_INPUT_TOKENS" = "120" && "#,
                    r#"test "$ORCA_USAGE_OUTPUT_TOKENS" = "30" && "#,
                    r#"test "$ORCA_USAGE_CACHE_TOKENS" = "10""#,
                ),
                concat!(
                    "if ($env:ORCA_USAGE_INPUT_TOKENS -ne '120' -or ",
                    "$env:ORCA_USAGE_OUTPUT_TOKENS -ne '30' -or ",
                    "$env:ORCA_USAGE_CACHE_TOKENS -ne '10') { exit 7 }",
                ),
            ),
            tool: None,
        }]);
        let usage = Usage {
            input_tokens: 120,
            output_tokens: 30,
            cache_tokens: 10,
        };

        runner
            .run(
                HookEvent::PostModelCall,
                HookContext {
                    cwd: ".",
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: None,
                    after_messages: None,
                    usage: Some(&usage),
                },
            )
            .unwrap();
    }
}
