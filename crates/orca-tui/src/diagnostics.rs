//! Presentation-only diagnostics for the TUI.
//!
//! Typed runtime terminals are authoritative. String classification exists
//! only to make legacy errors actionable and never changes execution policy,
//! retry behavior, or persisted state.

use orca_runtime::surface::{
    CancelReason, FailureClass, NotAdmittedReason, OperationBudget, OperationTerminal,
    SurfaceShutdownReason, TurnRequestBudgetScope,
};

const MAX_DIAGNOSTIC_DETAIL_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticLevel {
    Error,
    Warning,
    Info,
}

impl DiagnosticLevel {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warning => "WARNING",
            Self::Info => "INFO",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticContext {
    Runtime,
    Operation,
    Input,
    Interaction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiDiagnostic {
    level: DiagnosticLevel,
    code: &'static str,
    title: &'static str,
    detail: String,
    action: Option<&'static str>,
}

impl TuiDiagnostic {
    fn new(
        level: DiagnosticLevel,
        code: &'static str,
        title: &'static str,
        detail: impl Into<String>,
        action: Option<&'static str>,
    ) -> Self {
        let detail = detail.into();
        Self {
            level,
            code,
            title,
            detail: bounded_detail(if detail.trim().is_empty() {
                "No diagnostic detail was provided.".to_string()
            } else {
                detail
            }),
            action,
        }
    }

    pub(crate) fn from_message(context: DiagnosticContext, message: impl Into<String>) -> Self {
        let message = message.into();
        let lower = message.to_ascii_lowercase();
        let (code, title, action) = if contains_any(
            &lower,
            &[
                "sandbox",
                "seatbelt",
                "bubblewrap",
                "landlock",
                "seccomp",
                "enforcement",
            ],
        ) {
            (
                "shell.sandbox",
                "Shell sandbox unavailable",
                "Run `orca doctor` and repair the reported sandbox backend. `/trust` does not enable OS enforcement.",
            )
        } else if contains_any(
            &lower,
            &[
                "no space left",
                "failed to save",
                "failed to persist",
                "database",
                "sqlite",
                "ledger",
                "history",
            ],
        ) {
            (
                "runtime.persistence",
                "Session state could not be saved",
                "Check free disk space and filesystem permissions before retrying.",
            )
        } else if contains_any(
            &lower,
            &[
                "api key",
                "api_key",
                "authentication",
                "unauthorized",
                "status 401",
                "http 401",
                "status 403",
                "http 403",
            ],
        ) {
            (
                "provider.authentication",
                "Authentication failed",
                "Check the configured API key and endpoint, then retry.",
            )
        } else if contains_any(
            &lower,
            &[
                "context length",
                "context window",
                "context limit",
                "prompt too long",
            ],
        ) {
            (
                "provider.context_limit",
                "Model context limit reached",
                "Compact the conversation or start a new session, then retry.",
            )
        } else if contains_any(&lower, &["rate limit", "status 429", "http 429", "quota"]) {
            (
                "provider.rate_limit",
                "Provider limit reached",
                "Wait before retrying and check the provider account quota if the error persists.",
            )
        } else if contains_any(&lower, &["timed out", "timeout"]) {
            (
                "runtime.timeout",
                "Operation timed out",
                "Retry the operation or increase its configured timeout.",
            )
        } else if contains_any(
            &lower,
            &["permission", "not allowed", "denied", "policy conflict"],
        ) {
            (
                "permission.denied",
                "Permission denied",
                "Review the approval mode and active permission profile before retrying.",
            )
        } else if contains_any(
            &lower,
            &[
                "disconnected",
                "connection refused",
                "connection reset",
                "connection closed",
                "request for url",
                "http request",
                "dns",
                "network",
                "socket",
                "transport",
            ],
        ) {
            (
                "connection.failed",
                "Connection failed",
                "Check the network, proxy, and configured endpoint, then retry.",
            )
        } else if lower.contains("verification") {
            (
                "verification.failed",
                "Verification failed",
                "Inspect the verification output, fix the reported issue, and run verification again.",
            )
        } else if lower.contains("workflow") {
            (
                "workflow.failed",
                "Workflow failed",
                "Inspect the workflow task details and retry from the failed step.",
            )
        } else if lower.contains("hook") {
            (
                "hook.failed",
                "Hook failed",
                "Check the hook command and configuration, then retry.",
            )
        } else if contains_any(&lower, &["provider", "deepseek", "model response"]) {
            (
                "provider.failed",
                "Model request failed",
                "Retry once. If it persists, check the API endpoint, network, and provider status.",
            )
        } else if lower.contains("tool") {
            (
                "tool.failed",
                "Tool execution failed",
                "Inspect the tool target and arguments, then retry with corrected input.",
            )
        } else if contains_any(
            &lower,
            &[
                "unknown slash command",
                "unsupported mode",
                "unsupported plan command",
                "message exceeds",
                "image attachment",
                "nothing to compact",
                "nothing to backtrack",
                "before a session exists",
                "finish or cancel",
            ],
        ) {
            (
                "input.invalid",
                "Request could not be applied",
                "Correct the request or finish the conflicting operation, then try again.",
            )
        } else {
            match context {
                DiagnosticContext::Runtime => (
                    "runtime.failed",
                    "Runtime error",
                    "Retry once. If it happens again, run `/status` and `orca doctor`, then report this diagnostic.",
                ),
                DiagnosticContext::Operation => (
                    "operation.rejected",
                    "Request was not started",
                    "Resolve the reported state or policy conflict, then retry.",
                ),
                DiagnosticContext::Input => (
                    "input.invalid",
                    "Input was not accepted",
                    "Correct the input and submit it again.",
                ),
                DiagnosticContext::Interaction => (
                    "interaction.failed",
                    "Response was not accepted",
                    "Review the pending request and submit the response again.",
                ),
            }
        };
        Self::new(DiagnosticLevel::Error, code, title, message, Some(action))
    }

    pub(crate) fn from_surface_terminal(terminal: &OperationTerminal) -> Option<Self> {
        match terminal {
            OperationTerminal::Succeeded { .. } => None,
            OperationTerminal::Failed { class, message } => {
                let (code, title, action) = failure_presentation(*class);
                Some(Self::new(
                    DiagnosticLevel::Error,
                    code,
                    title,
                    message.as_str(),
                    Some(action),
                ))
            }
            OperationTerminal::Panicked { message } => Some(Self::new(
                DiagnosticLevel::Error,
                "runtime.panic",
                "Runtime task panicked",
                message.as_str(),
                Some(
                    "Restart the session. If it recurs, run `orca doctor` and report this diagnostic.",
                ),
            )),
            OperationTerminal::JoinFailed { message } => Some(Self::new(
                DiagnosticLevel::Error,
                "runtime.join_failed",
                "Runtime task could not be joined",
                message.as_str(),
                Some("Reload the session before retrying so the task state can be reconciled."),
            )),
            OperationTerminal::AbortedByRuntimeRestart { last_generation } => Some(Self::new(
                DiagnosticLevel::Warning,
                "runtime.restarted",
                "Task interrupted by runtime restart",
                format!(
                    "The runtime restarted after generation {}.",
                    last_generation.get()
                ),
                Some("Reload the session and inspect the recovered state before retrying."),
            )),
            OperationTerminal::BudgetExhausted { budget } => Some(budget_diagnostic(budget)),
            OperationTerminal::NotAdmitted { reason } => Some(not_admitted_diagnostic(*reason)),
            OperationTerminal::Cancelled { reason } => Some(match reason {
                CancelReason::User => Self::new(
                    DiagnosticLevel::Info,
                    "operation.cancelled_by_user",
                    "Task cancelled",
                    "The current task was cancelled before completion.",
                    Some("Submit the request again to retry."),
                ),
                CancelReason::GoalPause => Self::new(
                    DiagnosticLevel::Info,
                    "operation.goal_paused",
                    "Goal paused",
                    "The current task stopped because its goal was paused.",
                    Some("Resume the goal when you are ready to continue."),
                ),
            }),
            OperationTerminal::Shutdown { reason } => Some(match reason {
                SurfaceShutdownReason::HostShutdown => Self::new(
                    DiagnosticLevel::Warning,
                    "runtime.host_shutdown",
                    "Task stopped during runtime shutdown",
                    "The runtime host shut down before the task completed.",
                    Some(
                        "Reconnect or restart Orca, inspect the recovered session, and then retry.",
                    ),
                ),
                SurfaceShutdownReason::ThreadClose => Self::new(
                    DiagnosticLevel::Info,
                    "runtime.thread_closed",
                    "Task stopped because the conversation closed",
                    "The conversation was closed before the task completed.",
                    None,
                ),
            }),
        }
    }

    pub(crate) fn from_completion_status(status: &str) -> Option<Self> {
        match status {
            "success" | "completed" | "backgrounded" => None,
            "cancelled" | "interrupted" => Some(Self::new(
                DiagnosticLevel::Info,
                "operation.cancelled",
                "Task interrupted",
                "The task ended before completion, but no more specific reason was reported.",
                Some("Submit the request again to retry."),
            )),
            "budget_exhausted" => Some(Self::new(
                DiagnosticLevel::Warning,
                "budget.exhausted",
                "Task budget exhausted",
                "The task reached a configured execution limit.",
                Some("Review `/status` and increase the relevant budget before retrying."),
            )),
            "verification_failed" => Some(Self::new(
                DiagnosticLevel::Error,
                "verification.failed",
                "Verification failed",
                "The task output did not satisfy its verification gate.",
                Some(
                    "Inspect the verification output, fix the reported issue, and run verification again.",
                ),
            )),
            "not_admitted" => Some(Self::new(
                DiagnosticLevel::Error,
                "operation.not_admitted",
                "Task was not started",
                "The runtime rejected the task before execution and supplied no more specific reason.",
                Some("Check the active task, configuration, and permission policy, then retry."),
            )),
            "disconnected" => Some(Self::new(
                DiagnosticLevel::Warning,
                "connection.disconnected",
                "Connection lost",
                "The client disconnected before the task outcome was confirmed.",
                Some(
                    "Reconnect and inspect the restored session before resubmitting; Orca does not automatically resend uncertain prompts.",
                ),
            )),
            "failed" | "error" => Some(Self::new(
                DiagnosticLevel::Error,
                "runtime.missing_diagnostic",
                "Task failed without diagnostic details",
                "The runtime reported failure without an accompanying cause.",
                Some(
                    "Retry once. If it recurs, run `/status` and `orca doctor`, then report this diagnostic.",
                ),
            )),
            other => Some(Self::new(
                DiagnosticLevel::Warning,
                "runtime.unrecognized_terminal",
                "Task stopped",
                format!("The runtime reported terminal status `{other}` without more detail."),
                Some("Inspect `/status` before retrying."),
            )),
        }
    }

    pub(crate) fn level(&self) -> DiagnosticLevel {
        self.level
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    pub(crate) fn title(&self) -> &'static str {
        self.title
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    pub(crate) fn action(&self) -> Option<&str> {
        self.action
    }
}

fn failure_presentation(class: FailureClass) -> (&'static str, &'static str, &'static str) {
    match class {
        FailureClass::Provider => (
            "provider.failed",
            "Model request failed",
            "Retry once. If it persists, check the API endpoint, network, and provider status.",
        ),
        FailureClass::Tool => (
            "tool.failed",
            "Tool execution failed",
            "Inspect the failed tool output and arguments before retrying.",
        ),
        FailureClass::Hook => (
            "hook.failed",
            "Hook failed",
            "Check the hook command and configuration, then retry.",
        ),
        FailureClass::Workflow => (
            "workflow.failed",
            "Workflow failed",
            "Inspect the workflow task details and retry from the failed step.",
        ),
        FailureClass::Verification => (
            "verification.failed",
            "Verification failed",
            "Inspect the verification output, fix the reported issue, and run verification again.",
        ),
        FailureClass::InputResolution => (
            "input.resolution_failed",
            "Input could not be resolved",
            "Check referenced files, mentions, and image attachments before retrying.",
        ),
        FailureClass::ClientCapabilityUnavailable => (
            "client.capability_unavailable",
            "Client capability unavailable",
            "Use a client that supports the requested interaction or disable that integration.",
        ),
        FailureClass::LegacyApprovalRequired => (
            "permission.approval_unavailable",
            "Approval could not be requested",
            "Use an interactive client or adjust the permission policy explicitly.",
        ),
        FailureClass::RuntimeInvariant => (
            "runtime.invariant",
            "Runtime consistency check failed",
            "Reload the session. If it recurs, run `orca doctor` and report this diagnostic.",
        ),
        FailureClass::Persistence => (
            "runtime.persistence",
            "Session state could not be saved",
            "Check free disk space and filesystem permissions before retrying.",
        ),
        FailureClass::ExternalEffectAmbiguous => (
            "effect.ambiguous",
            "External effect is uncertain",
            "Inspect the target system before retrying; repeating the action may duplicate it.",
        ),
        FailureClass::RemoteResourceCleanupAmbiguous => (
            "cleanup.ambiguous",
            "Remote cleanup is uncertain",
            "Inspect the remote terminal or resource before starting another operation.",
        ),
    }
}

fn budget_diagnostic(budget: &OperationBudget) -> TuiDiagnostic {
    let (code, detail, action) = match budget {
        OperationBudget::ModelTokens { limit, observed } => (
            "budget.model_tokens",
            optional_budget_detail("Model token", *limit, *observed),
            "Start a new conversation or compact context before retrying.",
        ),
        OperationBudget::TurnRequests {
            scope,
            limit,
            observed,
        } => {
            let scope = match scope {
                TurnRequestBudgetScope::AgentLoop => "agent loop",
                TurnRequestBudgetScope::Subagent => "subagent",
            };
            (
                "budget.turn_requests",
                format!(
                    "The {scope} used {observed} turn requests; the configured limit is {limit}."
                ),
                "Increase the turn budget or narrow the task before retrying.",
            )
        }
        OperationBudget::ToolCalls { limit, observed } => (
            "budget.tool_calls",
            format!("The task used {observed} tool calls; the configured limit is {limit}."),
            "Increase the tool-call budget or narrow the task before retrying.",
        ),
        OperationBudget::WallTimeMs { limit, observed } => (
            "budget.wall_time",
            format!(
                "The task ran for {observed} ms; the configured wall-time limit is {limit} ms."
            ),
            "Increase the wall-time budget or narrow the task before retrying.",
        ),
        OperationBudget::GoalTokenBudget {
            limit, observed, ..
        } => (
            "budget.goal_tokens",
            format!("The goal used {observed} tokens; the configured limit is {limit}."),
            "Increase the goal token budget or narrow the goal before resuming.",
        ),
        OperationBudget::WorkflowTokenBudget {
            limit, observed, ..
        } => (
            "budget.workflow_tokens",
            format!("The workflow used {observed} tokens; the configured limit is {limit}."),
            "Increase the workflow token budget or narrow the workflow before retrying.",
        ),
        OperationBudget::MonetaryBudgetUsdMicros { limit, observed } => (
            "budget.cost",
            format!(
                "The task used {} micro-USD; the configured limit is {} micro-USD.",
                observed, limit
            ),
            "Increase the cost budget or narrow the task before retrying.",
        ),
    };
    TuiDiagnostic::new(
        DiagnosticLevel::Warning,
        code,
        "Task budget exhausted",
        detail,
        Some(action),
    )
}

fn optional_budget_detail(label: &str, limit: Option<u64>, observed: Option<u64>) -> String {
    match (limit, observed) {
        (Some(limit), Some(observed)) => {
            format!("{label} usage reached {observed}; the configured limit is {limit}.")
        }
        (Some(limit), None) => format!("The configured {label} limit of {limit} was reached."),
        (None, Some(observed)) => format!("{label} usage reached {observed}."),
        (None, None) => format!("The {label} budget was exhausted."),
    }
}

fn not_admitted_diagnostic(reason: NotAdmittedReason) -> TuiDiagnostic {
    let (detail, action) = match reason {
        NotAdmittedReason::CancelledBeforeAdmission => (
            "The task was cancelled before execution started.",
            "Submit the request again to retry.",
        ),
        NotAdmittedReason::ReservationExpired => (
            "The task reservation expired before execution started.",
            "Retry the request after the current workload settles.",
        ),
        NotAdmittedReason::ConfigurationConflict => (
            "Runtime settings changed before the task could start.",
            "Review the current settings and submit the request again.",
        ),
        NotAdmittedReason::PolicyConflict => (
            "The active permission policy rejected the task before execution.",
            "Review the approval mode and permission profile before retrying.",
        ),
        NotAdmittedReason::RuntimeRestart => (
            "The runtime restarted before the task could start.",
            "Reload the session and submit the request again.",
        ),
        NotAdmittedReason::HostShutdown => (
            "The runtime host shut down before the task could start.",
            "Restart or reconnect to Orca before retrying.",
        ),
        NotAdmittedReason::ThreadClose => (
            "The conversation closed before the task could start.",
            "Open or restore a conversation before retrying.",
        ),
    };
    TuiDiagnostic::new(
        DiagnosticLevel::Error,
        "operation.not_admitted",
        "Task was not started",
        detail,
        Some(action),
    )
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn bounded_detail(detail: String) -> String {
    let detail = detail.trim();
    if detail.len() <= MAX_DIAGNOSTIC_DETAIL_BYTES {
        return detail.to_string();
    }
    let mut end = MAX_DIAGNOSTIC_DETAIL_BYTES;
    while !detail.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}... [diagnostic truncated]", &detail[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_runtime::surface::SafeDiagnosticText;

    #[test]
    fn raw_errors_keep_the_cause_and_add_actionable_classification() {
        let diagnostic = TuiDiagnostic::from_message(
            DiagnosticContext::Runtime,
            "DeepSeek provider error: 429 rate limit exceeded",
        );

        assert_eq!(diagnostic.code(), "provider.rate_limit");
        assert_eq!(diagnostic.title(), "Provider limit reached");
        assert!(diagnostic.detail().contains("429"));
        assert!(diagnostic.action().unwrap().contains("quota"));
    }

    #[test]
    fn raw_error_classification_prefers_the_actionable_failure_boundary() {
        let persistence = TuiDiagnostic::from_message(
            DiagnosticContext::Runtime,
            "failed to save API key: No space left on device",
        );
        let permission = TuiDiagnostic::from_message(
            DiagnosticContext::Runtime,
            "network permission denied by active policy",
        );

        assert_eq!(persistence.code(), "runtime.persistence");
        assert_eq!(permission.code(), "permission.denied");
    }

    #[test]
    fn typed_terminal_preserves_failure_class_and_exact_detail() {
        let diagnostic = TuiDiagnostic::from_surface_terminal(&OperationTerminal::Failed {
            class: FailureClass::Persistence,
            message: SafeDiagnosticText::try_new("database is read-only").unwrap(),
        })
        .expect("failed terminal diagnostic");

        assert_eq!(diagnostic.code(), "runtime.persistence");
        assert_eq!(diagnostic.title(), "Session state could not be saved");
        assert_eq!(diagnostic.detail(), "database is read-only");
        assert!(diagnostic.action().unwrap().contains("disk space"));
    }

    #[test]
    fn every_typed_failure_class_has_a_stable_diagnostic_code() {
        for (class, expected_code) in [
            (FailureClass::Provider, "provider.failed"),
            (FailureClass::Tool, "tool.failed"),
            (FailureClass::Hook, "hook.failed"),
            (FailureClass::Workflow, "workflow.failed"),
            (FailureClass::Verification, "verification.failed"),
            (FailureClass::InputResolution, "input.resolution_failed"),
            (
                FailureClass::ClientCapabilityUnavailable,
                "client.capability_unavailable",
            ),
            (
                FailureClass::LegacyApprovalRequired,
                "permission.approval_unavailable",
            ),
            (FailureClass::RuntimeInvariant, "runtime.invariant"),
            (FailureClass::Persistence, "runtime.persistence"),
            (FailureClass::ExternalEffectAmbiguous, "effect.ambiguous"),
            (
                FailureClass::RemoteResourceCleanupAmbiguous,
                "cleanup.ambiguous",
            ),
        ] {
            let diagnostic = TuiDiagnostic::from_surface_terminal(&OperationTerminal::Failed {
                class,
                message: SafeDiagnosticText::try_new("exact cause").unwrap(),
            })
            .expect("failed terminal diagnostic");
            assert_eq!(diagnostic.code(), expected_code);
            assert_eq!(diagnostic.detail(), "exact cause");
            assert!(diagnostic.action().is_some());
        }
    }

    #[test]
    fn every_typed_budget_has_specific_observed_and_limit_detail() {
        let budgets = [
            OperationBudget::ModelTokens {
                limit: Some(100),
                observed: Some(101),
            },
            OperationBudget::TurnRequests {
                scope: TurnRequestBudgetScope::AgentLoop,
                limit: 2,
                observed: 3,
            },
            OperationBudget::ToolCalls {
                limit: 4,
                observed: 5,
            },
            OperationBudget::WallTimeMs {
                limit: 6,
                observed: 7,
            },
            OperationBudget::GoalTokenBudget {
                goal_id: orca_runtime::surface::SurfaceGoalId::try_new("goal-1").unwrap(),
                limit: 8,
                observed: 9,
            },
            OperationBudget::WorkflowTokenBudget {
                workflow_run_id: orca_runtime::surface::SurfaceWorkflowRunId::try_new("workflow-1")
                    .unwrap(),
                limit: 10,
                observed: 11,
            },
            OperationBudget::MonetaryBudgetUsdMicros {
                limit: 12,
                observed: 13,
            },
        ];

        for budget in budgets {
            let diagnostic =
                TuiDiagnostic::from_surface_terminal(&OperationTerminal::BudgetExhausted {
                    budget,
                })
                .expect("budget terminal diagnostic");
            assert!(diagnostic.code().starts_with("budget."));
            assert!(diagnostic.detail().contains("limit"));
            assert!(diagnostic.action().is_some());
        }
    }

    #[test]
    fn every_non_success_status_has_a_fallback_diagnostic() {
        for status in [
            "cancelled",
            "interrupted",
            "budget_exhausted",
            "verification_failed",
            "not_admitted",
            "disconnected",
            "failed",
            "error",
            "future_terminal",
        ] {
            assert!(
                TuiDiagnostic::from_completion_status(status).is_some(),
                "{status}"
            );
        }
        for status in ["success", "completed", "backgrounded"] {
            assert!(
                TuiDiagnostic::from_completion_status(status).is_none(),
                "{status}"
            );
        }
    }

    #[test]
    fn diagnostic_detail_is_utf8_safe_and_bounded() {
        let diagnostic =
            TuiDiagnostic::from_message(DiagnosticContext::Runtime, "界".repeat(6_000));

        assert!(diagnostic.detail().len() <= MAX_DIAGNOSTIC_DETAIL_BYTES + 27);
        assert!(diagnostic.detail().ends_with("[diagnostic truncated]"));
    }
}
