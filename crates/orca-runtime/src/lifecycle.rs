use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::event_schema::{EventDraft, EventFactory, RunStatus};
use orca_core::external_config::ExternalToolConfig;
use orca_core::hook_types::HookEvent;
use orca_core::model::{ModelRouteContext, ModelRouteDecision, ModelSelection};
use orca_core::provider_types::{ProviderResponse, ProviderStep, Usage};
use orca_core::subagent_types::SubagentType;
use orca_core::thread_identity::TurnId;
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult, ToolStatus};
use orca_core::{
    cancel::CancelToken,
    config::{ProviderKind, RunConfig},
    conversation::Conversation,
};
use orca_mcp::{McpElicitationHandler, McpRegistry};
use orca_provider::ProviderConfig;
use serde_json::Value;

use crate::background_turn::RuntimeTurnContinuation;
use crate::cost::CostTracker;
use crate::extension::{
    ExtensionData, ExtensionRegistry, ExtensionRegistryBuilder, RuntimeExtensionContext,
    RuntimeExtensionStores,
};
use crate::hooks::{HookContext, HookOutcome, HookRunError, HookRunner};
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;
use crate::provider_stream::RuntimeProviderSuspensionControl;
use crate::runtime_directive::{RuntimeDirective, RuntimeDirectiveState};
use crate::runtime_state::RuntimeTurnReducer;
use crate::runtime_surface::{RuntimeProviderResponseIngress, RuntimeWorkflowLifecycleIngress};
use crate::runtime_tool_call::{
    RuntimeNormalToolInteractions, RuntimeNormalToolInvocation, RuntimeToolCallRuntime,
};
use crate::runtime_turn_kernel::RuntimeTurnKernel;
use crate::tasks::TaskRegistry;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) use crate::runtime_approval::RuntimeToolApprovalPolicy;
pub use crate::runtime_approval::{
    RuntimeApprovalDecision, RuntimeApprovalHandler, RuntimeConfigApprovalHandler,
};
pub use crate::runtime_lifecycle::{
    RuntimeAdvancedTurn, RuntimeSessionLifecycle, RuntimeStartedTurn, RuntimeTaskKind,
    RuntimeTaskLifecycle, RuntimeTaskStatus, RuntimeTurnLifecycle, RuntimeTurnRunner,
};
pub(crate) use crate::runtime_permission::AllowRequestedPermissions;
pub use crate::runtime_permission::{
    RuntimePermissionContext, RuntimePermissionRequest, RuntimePermissionRequestHandler,
    RuntimePermissionResponse, TurnPermissionOverlay,
};
pub use crate::runtime_special::{RuntimeSpecialToolDispatch, RuntimeWorkflowDraftRequest};
pub use crate::runtime_tool_actor::RuntimeToolActorContext;
pub use crate::runtime_user_input::{RuntimeUserInputHandler, RuntimeUserInputRequest};

pub struct RuntimeTaskActor<'a> {
    lifecycle: &'a mut RuntimeSessionLifecycle,
    turns_started: u32,
    /// Cost thresholds already acknowledged within this outer turn.
    cost_budget_reminder_index: u32,
    /// Reminder produced after the latest provider usage update and consumed
    /// by the next model-turn opening.
    pending_cost_budget_soft_landing: Option<String>,
}

pub(crate) fn run_status_from_tool_status(status: ToolStatus) -> RunStatus {
    match status {
        ToolStatus::Completed => RunStatus::Success,
        ToolStatus::Denied => RunStatus::ApprovalRequired,
        ToolStatus::Cancelled => RunStatus::Cancelled,
        ToolStatus::Failed | ToolStatus::NotImplemented | ToolStatus::Indeterminate => {
            RunStatus::Failed
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentLoopResult {
    /// Legacy projection kept for adapters that have not migrated yet; the
    /// typed [`terminal`](Self::terminal) is the fact and every legacy
    /// field is derived from it, never reconstructed independently.
    pub status: RunStatus,
    /// Legacy projection; see [`status`](Self::status).
    pub reason: TurnEndReason,
    pub final_message: Option<String>,
    pub error: Option<String>,
    /// The one typed operation terminal every loop produces. Budget stops
    /// commit it durably (checkpoint + terminal) before surfacing; every
    /// other exit derives it from the outcome status, and the operation
    /// context fills final usage at loop exit.
    pub terminal: orca_core::budget::OperationTerminal,
}

pub(crate) enum AgentLoopOutcome {
    Completed(AgentLoopResult),
    ProviderSuspended(crate::provider_stream::RuntimeProviderSuspension),
}

impl AgentLoopResult {
    pub(crate) fn success(final_message: Option<String>) -> Self {
        Self {
            status: RunStatus::Success,
            reason: TurnEndReason::Unclassified,
            final_message,
            error: None,
            terminal: orca_core::budget::OperationTerminal::Completed {
                usage: orca_core::budget::BudgetUsage::default(),
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn failure(status: RunStatus, error: impl Into<String>) -> Self {
        Self::terminal(status, TurnEndReason::Unclassified, Some(error.into()))
    }

    pub(crate) fn terminal(
        status: RunStatus,
        reason: TurnEndReason,
        error: Option<String>,
    ) -> Self {
        let terminal = Self::terminal_from_status(status, error.as_deref());
        Self {
            status,
            reason,
            final_message: None,
            error,
            terminal,
        }
    }

    /// Derives the typed terminal from the legacy outcome status. This is the
    /// single reconstruction site for non-budget exits; budget stops always
    /// carry their committed terminal instead.
    fn terminal_from_status(
        status: RunStatus,
        error: Option<&str>,
    ) -> orca_core::budget::OperationTerminal {
        use orca_core::budget::{CancelReason, FailureClass};
        match status {
            RunStatus::Success => orca_core::budget::OperationTerminal::Completed {
                usage: orca_core::budget::BudgetUsage::default(),
            },
            RunStatus::Cancelled => orca_core::budget::OperationTerminal::Cancelled {
                reason: CancelReason::User,
                checkpoint_id: None,
            },
            RunStatus::VerificationFailed => orca_core::budget::OperationTerminal::Failed {
                class: FailureClass::Verification,
                message: error.unwrap_or_default().to_string(),
            },
            RunStatus::Failed | RunStatus::ApprovalRequired => {
                orca_core::budget::OperationTerminal::Failed {
                    class: FailureClass::Runtime,
                    message: error.unwrap_or_default().to_string(),
                }
            }
        }
    }

    /// Typed budget stop produced by the operation's `BudgetController`;
    /// no further provider request is made after this result. The terminal
    /// carries the durable checkpoint committed by the operation context, and
    /// the legacy status/reason fields are projections; the typed terminal is
    /// the fact.
    pub fn budget_stop(
        terminal: orca_core::budget::OperationTerminal,
        stop: orca_core::budget::BudgetStop,
    ) -> Self {
        Self {
            status: RunStatus::Failed,
            reason: TurnEndReason::CostBudgetExhausted,
            final_message: None,
            error: Some(format!(
                "budget stopped: {} (turns={}, tool_calls={})",
                stop.reason.as_str(),
                stop.usage.turns,
                stop.usage.tool_calls
            )),
            terminal,
        }
    }
}

impl From<RuntimeTurnStartError> for AgentLoopResult {
    fn from(error: RuntimeTurnStartError) -> Self {
        // A terminal already committed to the execution journal (durable
        // checkpoint + terminal) is the fact; otherwise derive it from the
        // outcome status. Legacy status/reason are projections of it.
        let terminal = error
            .terminal
            .unwrap_or_else(|| Self::terminal_from_status(error.status, Some(&error.message)));
        let mut result = Self::terminal(error.status, error.reason, Some(error.message));
        result.terminal = terminal;
        result
    }
}

#[derive(Clone, Debug, Default)]
pub struct ThreadSteerHandle {
    pending: Arc<Mutex<Vec<String>>>,
}

pub(crate) struct AgentLoopContext<'a> {
    pub(crate) turn_context: RuntimeTurnContext<'a>,
    pub(crate) turn_deps: Option<RuntimeTurnDeps<'a>>,
    pub(crate) turn_state: Option<RuntimeTurnState<'a>>,
    pub(crate) turn_execution: Option<RuntimeTurnExecution<'a>>,
}

#[derive(Clone)]
pub(crate) struct RuntimeTurnContext<'a> {
    pub(crate) turn_id: TurnId,
    pub(crate) cwd: &'a Path,
    pub(crate) prompt: &'a str,
    pub(crate) subagent_depth: u32,
    pub(crate) emit_deltas: bool,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) root_task_id: Option<&'a str>,
    pub(crate) continuation: Option<RuntimeTurnContinuation>,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
    pub(crate) provider_suspension_control: Option<&'a dyn RuntimeProviderSuspensionControl>,
    pub(crate) provider_response_ingress: Option<&'a dyn RuntimeProviderResponseIngress>,
    pub(crate) workflow_lifecycle_ingress: Option<&'a dyn RuntimeWorkflowLifecycleIngress>,
    pub(crate) wait_for_background_workflows: bool,
    pub(crate) defer_cancel_terminal: bool,
    /// The handler is owned by the turn so a synchronous child invocation can
    /// move an `Arc` across its worker boundary without borrowing the parent.
    pub(crate) permission_handler_owned:
        Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
}

#[derive(Clone, Copy)]
pub(crate) struct RuntimeTurnDeps<'a> {
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) turn_interactions: RuntimeTurnInteractionState<'a>,
}

pub(crate) struct RuntimeTurnState<'a> {
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) directive_state: RuntimeDirectiveState,
    pub(crate) extension_registry: ExtensionRegistry,
    pub(crate) thread_extensions: Arc<ExtensionData>,
    pub(crate) turn_extensions: Arc<ExtensionData>,
}

pub(crate) struct RuntimeTurnLoopState<'a> {
    pub(crate) directive_state: RuntimeDirectiveState,
    pub(crate) runtime: RuntimeTurnLoopRuntime<'a>,
}

pub(crate) struct RuntimeTurnLoopRuntime<'a> {
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) extensions: RuntimeTurnExtensionState,
}

pub(crate) struct RuntimeTurnLoopIterationState<'a> {
    pub(crate) runtime_system_messages: &'a [String],
    pub(crate) model_override: Option<&'a str>,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) extensions: RuntimeExtensionContext<'a>,
}

pub(crate) struct RuntimeTurnExtensionState {
    extension_registry: ExtensionRegistry,
    thread_extensions: Arc<ExtensionData>,
    turn_extensions: Arc<ExtensionData>,
}

pub(crate) struct RuntimeTurnExecution<'a> {
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct RuntimeTurnInteractionState<'a> {
    approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
    permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
}

#[derive(Clone)]
pub struct RuntimeModelTurn {
    pub decision: ModelRouteDecision,
    pub provider_config: ProviderConfig,
}

#[derive(Debug)]
pub struct RuntimeActorStartedTurn {
    turn: u32,
    task: Option<RuntimeTaskLifecycle>,
    event: Option<EventDraft>,
}

/// Why an outer turn ended, kept separate from [`RunStatus`] because one status
/// is reachable for reasons that differ in whether resuming can make progress:
/// a typed budget stop with a committed checkpoint resumes cleanly, while a
/// cost ceiling would re-trip immediately under unchanged config. Budget
/// specifics live in the typed `OperationTerminal`, not here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TurnEndReason {
    /// The configured cost budget ceiling (in USD micros) was exceeded.
    CostBudgetExhausted,
    /// The turn was explicitly cancelled.
    Cancelled,
    /// Not yet classified; treated as not automatically resumable.
    #[default]
    Unclassified,
}

impl TurnEndReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CostBudgetExhausted => "cost_budget_exhausted",
            Self::Cancelled => "cancelled",
            Self::Unclassified => "unclassified",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeTurnStartError {
    pub status: RunStatus,
    pub reason: TurnEndReason,
    pub message: String,
    /// The cost ceiling (USD micros) that caused a `CostBudgetExhausted`
    /// stop, so adapters never reconstruct limits from constants.
    pub max_cost_usd_micros: u64,
    /// Typed operation terminal already committed to the execution journal
    /// (a budget stop with a durable checkpoint). When present it wins over
    /// any legacy status/reason reconstruction; `None` keeps legacy behavior.
    pub terminal: Option<orca_core::budget::OperationTerminal>,
}

pub trait RuntimeWorkflowIpc {
    fn send_message(
        &self,
        channel: &str,
        from: Option<&str>,
        message: Value,
    ) -> Result<Value, String>;
    fn read_messages(&self, channel: &str) -> Result<Value, String>;
    fn clear_messages(&self, channel: &str) -> Result<Value, String>;
    fn create_task_list(&self, name: &str, items: Vec<Value>) -> Result<Value, String>;
    fn claim_task(&self, name: &str, by: Option<&str>) -> Result<Value, String>;
    fn complete_task(
        &self,
        name: &str,
        task_id: &str,
        result: Value,
        by: Option<&str>,
    ) -> Result<Value, String>;
    fn list_tasks(&self, name: &str) -> Result<Value, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSubagentStatusRecord {
    pub id: String,
    pub status: String,
    pub description: String,
    pub agent_type: Option<String>,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub usage: Option<RuntimeUsageTotals>,
    pub subagent_current_activity: Option<String>,
    pub subagent_turn: Option<u32>,
    pub last_activity_at_ms: Option<i64>,
    pub continuation_id: Option<String>,
    pub continuation_attempt_id: Option<String>,
    pub continuation_checkpoint_id: Option<String>,
    pub continuation_resumable: bool,
    pub continuation_indeterminate: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RuntimeUsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub estimated_cost_usd: f64,
}

pub trait RuntimeSubagentStatusLookup {
    fn subagent_status_record(&self, agent_id: &str) -> Option<RuntimeSubagentStatusRecord>;
}

impl<'a> RuntimeTaskActor<'a> {
    pub fn new(lifecycle: &'a mut RuntimeSessionLifecycle) -> Self {
        let turns_started = lifecycle
            .active_task()
            .map(RuntimeTaskLifecycle::current_turn)
            .unwrap_or(0);
        Self {
            lifecycle,
            turns_started,
            cost_budget_reminder_index: 0,
            pending_cost_budget_soft_landing: None,
        }
    }

    pub fn turns_started(&self) -> u32 {
        self.turns_started
    }

    pub fn take_pending_cost_budget_soft_landing(&mut self) -> Option<String> {
        self.pending_cost_budget_soft_landing.take()
    }

    pub fn start_turn(
        &mut self,
        events: &mut EventFactory,
        turn_id: &TurnId,
        prompt: Option<&str>,
        emit_event: bool,
    ) -> Result<RuntimeActorStartedTurn, RuntimeTurnStartError> {
        // No implicit turn ceiling exists: the operation's BudgetController
        // admits turns and returns a typed stop instead.
        self.turns_started = self.turns_started.saturating_add(1);
        let started = if emit_event {
            let started =
                RuntimeTurnRunner::new(self.lifecycle).start_turn(events, turn_id, prompt);
            RuntimeActorStartedTurn {
                turn: started.turn,
                task: started.task,
                event: Some(started.event),
            }
        } else {
            let advanced = RuntimeTurnRunner::new(self.lifecycle).advance_turn();
            RuntimeActorStartedTurn {
                turn: advanced.turn,
                task: advanced.task,
                event: None,
            }
        };
        Ok(started)
    }

    pub fn active_task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.lifecycle.active_task()
    }

    pub fn route_model_turn(
        &mut self,
        model: &ModelSelection,
        subagent_type: &SubagentType,
        subagent_model: Option<&str>,
        has_images: bool,
        provider_config: &ProviderConfig,
        cost_tracker: &mut CostTracker,
    ) -> RuntimeModelTurn {
        let decision = model.route(ModelRouteContext {
            subagent_type,
            subagent_model,
            has_images,
        });
        cost_tracker.set_model(Some(&decision.actual_model));
        let mut provider_config = provider_config.clone();
        provider_config.model = Some(decision.actual_model.clone());
        RuntimeModelTurn {
            decision,
            provider_config,
        }
    }

    pub fn run_pre_model_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
    ) -> Result<HookOutcome, RuntimeTurnStartError> {
        self.run_pre_model_hook_with_cancel(hooks, cwd, None)
    }

    pub fn run_pre_model_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        cancel: Option<&CancelToken>,
    ) -> Result<HookOutcome, RuntimeTurnStartError> {
        let context = HookContext {
            cwd,
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        };
        let result = if let Some(cancel) = cancel {
            hooks.run_with_cancel(HookEvent::PreModelCall, context, cancel)
        } else {
            hooks.run(HookEvent::PreModelCall, context)
        };
        result.map_err(|error| {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                RuntimeTurnStartError {
                    status: RunStatus::Cancelled,
                    reason: TurnEndReason::Cancelled,
                    message: "turn cancelled".to_string(),
                    max_cost_usd_micros: 0,
                    terminal: None,
                }
            } else {
                RuntimeTurnStartError {
                    status: RunStatus::Failed,
                    reason: TurnEndReason::Unclassified,
                    message: format!("pre_model_call hook failed: {error}"),
                    max_cost_usd_micros: 0,
                    terminal: None,
                }
            }
        })
    }

    pub fn run_post_model_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        usage: Option<&Usage>,
    ) -> Option<String> {
        self.run_post_model_hook_with_cancel(hooks, cwd, usage, None)
    }

    pub fn run_post_model_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        usage: Option<&Usage>,
        cancel: Option<&CancelToken>,
    ) -> Option<String> {
        let context = HookContext {
            cwd,
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage,
        };
        let result = if let Some(cancel) = cancel {
            hooks.run_with_cancel(HookEvent::PostModelCall, context, cancel)
        } else {
            hooks.run(HookEvent::PostModelCall, context)
        };
        if cancel.is_some_and(CancelToken::is_cancelled) {
            None
        } else {
            result
                .err()
                .map(|error| format!("post_model_call hook failed: {error}"))
        }
    }

    pub fn call_streaming_provider(
        &mut self,
        kind: ProviderKind,
        conversation: &Conversation,
        provider_config: &ProviderConfig,
        cancel: &CancelToken,
        on_step: &mut dyn FnMut(&ProviderStep),
    ) -> ProviderResponse {
        orca_provider::call_streaming(kind, conversation, provider_config, cancel, on_step)
    }

    pub fn tool_call_requested_event(
        &mut self,
        events: &mut EventFactory,
        request: &ToolRequest,
    ) -> EventDraft {
        Self::tool_call_requested_event_for(events, request)
    }

    pub fn tool_call_completed_event(
        &mut self,
        events: &mut EventFactory,
        request: &ToolRequest,
        result: &ToolResult,
    ) -> EventDraft {
        Self::tool_call_completed_event_for(events, request, result)
    }

    pub fn tool_call_requested_event_for(
        events: &mut EventFactory,
        request: &ToolRequest,
    ) -> EventDraft {
        let event = events.tool_call_requested(request);
        attach_shell_task_to_tool_event(event, request, RuntimeTaskStatus::Running)
    }

    pub fn tool_call_completed_event_for(
        events: &mut EventFactory,
        request: &ToolRequest,
        result: &ToolResult,
    ) -> EventDraft {
        let status = RuntimeTaskStatus::from_run_status(run_status_from_tool_status(result.status));
        let event = events.tool_call_completed(result);
        attach_shell_task_to_tool_event(event, request, status)
    }

    pub fn run_pre_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
    ) -> Result<HookOutcome, ToolResult> {
        self.run_pre_tool_hook_with_cancel(hooks, cwd, request, None)
    }

    pub fn run_pre_tool_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
        cancel: Option<&CancelToken>,
    ) -> Result<HookOutcome, ToolResult> {
        let context = HookContext {
            cwd,
            session_status: None,
            tool_request: Some(request),
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        };
        let result = if let Some(cancel) = cancel {
            hooks.run_with_cancel_result(HookEvent::PreToolUse, context, cancel)
        } else {
            hooks
                .run(HookEvent::PreToolUse, context)
                .map_err(HookRunError::Failed)
        };
        result.map_err(|error| match error {
            HookRunError::Cancelled(_) => {
                ToolResult::cancelled_before_start(request, "the pre-tool hook was cancelled")
            }
            HookRunError::Failed(error) => ToolResult::failed_before_start(
                request,
                format!("pre_tool_use hook blocked tool: {error}"),
                None,
            ),
        })
    }

    pub fn run_post_tool_hook(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
        result: &ToolResult,
    ) -> Option<String> {
        self.run_post_tool_hook_with_cancel(hooks, cwd, request, result, None)
    }

    pub fn run_post_tool_hook_with_cancel(
        &mut self,
        hooks: &HookRunner,
        cwd: &str,
        request: &ToolRequest,
        result: &ToolResult,
        cancel: Option<&CancelToken>,
    ) -> Option<String> {
        let context = HookContext {
            cwd,
            session_status: None,
            tool_request: Some(request),
            tool_result: Some(result),
            before_messages: None,
            after_messages: None,
            usage: None,
        };
        let hook_result = if let Some(cancel) = cancel {
            hooks.run_with_cancel(HookEvent::PostToolUse, context, cancel)
        } else {
            hooks.run(HookEvent::PostToolUse, context)
        };
        if cancel.is_some_and(CancelToken::is_cancelled) {
            None
        } else {
            hook_result
                .err()
                .map(|error| format!("post_tool_use hook failed: {error}"))
        }
    }

    pub fn resolve_tool_approval(
        &mut self,
        policy: &ApprovalPolicy,
        approval: Option<ApprovalRequest>,
        request: &ToolRequest,
    ) -> RuntimeApprovalDecision {
        let Some(approval) = approval else {
            return RuntimeApprovalDecision::NotRequired;
        };
        let resolution =
            policy.resolve_for_tool(&approval, request.name.as_str(), request.target.as_deref());
        match resolution.decision {
            ApprovalDecision::Allow => RuntimeApprovalDecision::Allowed(resolution),
            ApprovalDecision::Ask => RuntimeApprovalDecision::Ask(approval),
            ApprovalDecision::Deny => {
                let result = ToolResult::denied(request, resolution.reason.clone());
                RuntimeApprovalDecision::Denied { resolution, result }
            }
        }
    }

    pub fn resolve_interactive_tool_approval(
        &mut self,
        handler: &dyn RuntimeApprovalHandler,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        handler.resolve_interactive(approval, request)
    }

    pub fn execute_normal_tool(
        &mut self,
        config: &RunConfig,
        request: &ToolRequest,
        cwd: &Path,
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
    ) -> ToolResult {
        self.execute_normal_tool_with_cancel(
            config,
            request,
            cwd,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_normal_tool_with_cancel(
        &mut self,
        config: &RunConfig,
        request: &ToolRequest,
        cwd: &Path,
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        cancel: Option<&CancelToken>,
    ) -> ToolResult {
        self.execute_normal_tool_with_roots_and_cancel(
            config,
            request,
            cwd,
            &[],
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            cancel,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_normal_tool_with_roots_and_cancel(
        &mut self,
        config: &RunConfig,
        request: &ToolRequest,
        cwd: &Path,
        additional_roots: &[PathBuf],
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
        cancel: Option<&CancelToken>,
        permission_handler: Option<&dyn RuntimePermissionRequestHandler>,
    ) -> ToolResult {
        let invocation = RuntimeNormalToolInvocation::snapshot(
            config,
            request,
            cwd,
            additional_roots,
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            TurnPermissionOverlay::default(),
        );
        let fallback_cancel = CancelToken::new();
        let parent_cancel = cancel.unwrap_or(&fallback_cancel);
        let runtime = RuntimeToolCallRuntime::for_normal_execution();
        match runtime.execute_normal(
            invocation,
            parent_cancel,
            RuntimeNormalToolInteractions {
                output_handler: None,
                permission_handler,
                mcp_elicitation_handler: None,
            },
        ) {
            Ok(output) => output.result,
            Err(error) => ToolResult::failed_before_start(
                request,
                format!("failed to execute normal tool: {error}"),
                None,
            ),
        }
    }

    pub fn execute_user_input_tool(
        &mut self,
        request: &ToolRequest,
        handler: &dyn RuntimeUserInputHandler,
    ) -> io::Result<ToolResult> {
        match crate::runtime_user_input::execute_user_input_tool(request, handler) {
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                Ok(ToolResult::invalid_input(request, error.to_string()))
            }
            result => result,
        }
    }

    pub fn record_usage(
        &mut self,
        usage: Usage,
        cost_tracker: &mut CostTracker,
        max_cost_usd_micros: Option<u64>,
    ) -> Result<orca_core::cost_types::UsageTotals, RuntimeTurnStartError> {
        let totals = cost_tracker.add_usage(usage);
        let spent_usd_micros = crate::cost::usd_to_micros(totals.estimated_cost_usd);
        if let Some(max_cost_usd_micros) = max_cost_usd_micros
            && spent_usd_micros > max_cost_usd_micros
        {
            return Err(RuntimeTurnStartError {
                status: RunStatus::Failed,
                reason: TurnEndReason::CostBudgetExhausted,
                message: format!(
                    "budget stopped: estimated cost ${:.6} exceeded limit ${:.6}",
                    totals.estimated_cost_usd,
                    max_cost_usd_micros as f64 / 1_000_000.0
                ),
                max_cost_usd_micros,
                terminal: None,
            });
        }
        if let Some(max_cost_usd_micros) = max_cost_usd_micros
            && let Some(reminder) = crate::budget_soft_landing::pending_cost_budget_reminder(
                max_cost_usd_micros,
                spent_usd_micros,
                self.cost_budget_reminder_index,
            )
        {
            self.cost_budget_reminder_index = reminder.reminder_index;
            self.pending_cost_budget_soft_landing = Some(
                crate::budget_soft_landing::format_soft_landing_message(&reminder),
            );
        }
        Ok(totals)
    }
}

impl ThreadSteerHandle {
    pub fn push(&self, input: impl Into<String>) {
        self.pending
            .lock()
            .expect("thread steer handle lock")
            .push(input.into());
    }

    pub fn drain(&self) -> Vec<String> {
        self.pending
            .lock()
            .expect("thread steer handle lock")
            .drain(..)
            .collect()
    }

    pub fn has_pending(&self) -> bool {
        !self
            .pending
            .lock()
            .expect("thread steer handle lock")
            .is_empty()
    }
}

impl<'a> AgentLoopContext<'a> {
    pub fn new(
        cwd: &'a Path,
        prompt: &'a str,
        subagent_depth: u32,
        emit_deltas: bool,
        subagent_type: &'a SubagentType,
    ) -> Self {
        Self {
            turn_context: RuntimeTurnContext::new(
                cwd,
                prompt,
                subagent_depth,
                emit_deltas,
                subagent_type,
            ),
            turn_deps: None,
            turn_state: None,
            turn_execution: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn turn_context(&self) -> RuntimeTurnContext<'a> {
        self.turn_context.clone()
    }

    pub fn with_services(
        mut self,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
    ) -> Self {
        self.turn_deps = Some(RuntimeTurnDeps::new(
            instructions,
            memory,
            mcp_registry,
            hooks,
        ));
        self
    }

    pub fn with_turn_id(mut self, turn_id: TurnId) -> Self {
        self.turn_context = self.turn_context.with_turn_id(turn_id);
        self
    }

    pub(crate) fn with_deferred_cancel_terminal(mut self, defer: bool) -> Self {
        self.turn_context = self.turn_context.with_deferred_cancel_terminal(defer);
        self
    }

    pub(crate) fn with_root_task_id(mut self, root_task_id: Option<&'a str>) -> Self {
        self.turn_context = self.turn_context.with_root_task_id(root_task_id);
        self
    }

    #[cfg(test)]
    pub(crate) fn turn_deps(&self) -> RuntimeTurnDeps<'a> {
        self.turn_deps.expect("agent loop turn deps")
    }

    #[cfg(test)]
    pub fn instructions(&self) -> &'a ProjectInstructions {
        self.turn_deps().instructions()
    }

    #[cfg(test)]
    pub fn memory(&self) -> &'a MemoryBlock {
        self.turn_deps().memory()
    }

    #[cfg(test)]
    pub fn mcp_registry(&self) -> &'a McpRegistry {
        self.turn_deps().mcp_registry()
    }

    #[cfg(test)]
    pub fn hooks(&self) -> &'a HookRunner {
        self.turn_deps().hooks()
    }

    pub fn with_runtime(
        mut self,
        cost_tracker: &'a mut CostTracker,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
    ) -> Self {
        self.turn_state = Some(RuntimeTurnState::new(cost_tracker, cancel, task_registry));
        self
    }

    pub(crate) fn with_runtime_thread_extensions(
        mut self,
        cost_tracker: &'a mut CostTracker,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
        thread_extensions: Arc<ExtensionData>,
        turn_extension_id: impl Into<String>,
    ) -> Self {
        self.turn_state = Some(RuntimeTurnState::new_with_thread_extensions(
            cost_tracker,
            cancel,
            task_registry,
            thread_extensions,
            turn_extension_id,
        ));
        self
    }

    #[cfg(test)]
    pub(crate) fn turn_state(&self) -> &RuntimeTurnState<'a> {
        self.turn_state.as_ref().expect("agent loop turn state")
    }

    #[cfg(test)]
    pub fn cost_tracker(&self) -> &CostTracker {
        self.turn_state().cost_tracker()
    }

    #[cfg(test)]
    pub fn cancel(&self) -> &'a CancelToken {
        self.turn_state().cancel()
    }

    #[cfg(test)]
    pub fn task_registry(&self) -> &'a TaskRegistry {
        self.turn_state().task_registry()
    }

    pub(crate) fn with_execution(
        mut self,
        background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
    ) -> Self {
        self.turn_execution = Some(RuntimeTurnExecution::new(
            background_workflows,
            workflow_ipc,
            lifecycle,
        ));
        self
    }

    pub(crate) fn with_steer_handle(mut self, steer_handle: Option<&'a ThreadSteerHandle>) -> Self {
        self.turn_context = self.turn_context.with_steer_handle(steer_handle);
        self
    }

    pub(crate) fn with_provider_suspension_control(
        mut self,
        control: Option<&'a dyn RuntimeProviderSuspensionControl>,
    ) -> Self {
        self.turn_context = self.turn_context.with_provider_suspension_control(control);
        self
    }

    pub(crate) fn with_owned_permission_handler(
        mut self,
        handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    ) -> Self {
        self.turn_context = self.turn_context.with_owned_permission_handler(handler);
        self
    }

    pub(crate) fn with_provider_response_ingress(
        mut self,
        ingress: Option<&'a dyn RuntimeProviderResponseIngress>,
    ) -> Self {
        self.turn_context = self.turn_context.with_provider_response_ingress(ingress);
        self
    }

    pub(crate) fn with_workflow_lifecycle_ingress(
        mut self,
        ingress: Option<&'a dyn RuntimeWorkflowLifecycleIngress>,
    ) -> Self {
        self.turn_context = self.turn_context.with_workflow_lifecycle_ingress(ingress);
        self
    }

    pub(crate) fn with_wait_for_background_workflows(mut self, wait: bool) -> Self {
        self.turn_context = self.turn_context.with_wait_for_background_workflows(wait);
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_initial_response(mut self, response: ProviderResponse) -> Self {
        let turn_id = self.turn_context.turn_id.clone();
        self.turn_context = self
            .turn_context
            .with_continuation(RuntimeTurnContinuation::from_response(response, turn_id));
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_turn_continuation(mut self, continuation: RuntimeTurnContinuation) -> Self {
        self.turn_context = self.turn_context.with_continuation(continuation);
        self
    }

    #[cfg(test)]
    pub(crate) fn initial_response(&self) -> Option<&ProviderResponse> {
        self.turn_context.initial_response()
    }

    #[cfg(test)]
    pub(crate) fn continuation(&self) -> Option<&RuntimeTurnContinuation> {
        self.turn_context.continuation()
    }

    pub(crate) fn with_permission_handler(
        mut self,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    ) -> Self {
        self.turn_deps = Some(
            self.turn_deps
                .expect("agent loop turn deps")
                .with_permission_handler(permission_handler),
        );
        self
    }

    pub(crate) fn with_approval_handler(
        mut self,
        approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
    ) -> Self {
        self.turn_deps = Some(
            self.turn_deps
                .expect("agent loop turn deps")
                .with_approval_handler(approval_handler),
        );
        self
    }

    pub(crate) fn with_user_input_handler(
        mut self,
        user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    ) -> Self {
        self.turn_deps = Some(
            self.turn_deps
                .expect("agent loop turn deps")
                .with_user_input_handler(user_input_handler),
        );
        self
    }

    pub(crate) fn with_mcp_elicitation_handler(
        mut self,
        mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    ) -> Self {
        self.turn_deps = Some(
            self.turn_deps
                .expect("agent loop turn deps")
                .with_mcp_elicitation_handler(mcp_elicitation_handler),
        );
        self
    }

    #[cfg(test)]
    pub(crate) fn turn_interactions(&self) -> RuntimeTurnInteractionState<'a> {
        self.turn_deps().turn_interactions()
    }

    #[cfg(test)]
    pub fn background_workflow_count(&self) -> usize {
        self.turn_execution().background_workflow_count()
    }

    #[cfg(test)]
    pub fn workflow_ipc(&self) -> Option<&'a WorkflowIpcContext> {
        self.turn_execution().workflow_ipc()
    }

    #[cfg(test)]
    pub fn lifecycle(&self) -> Option<&RuntimeSessionLifecycle> {
        self.turn_execution().lifecycle()
    }

    #[cfg(test)]
    pub(crate) fn turn_execution(&self) -> &RuntimeTurnExecution<'a> {
        self.turn_execution
            .as_ref()
            .expect("agent loop turn execution")
    }
}

impl<'a> RuntimeTurnContext<'a> {
    pub(crate) fn new(
        cwd: &'a Path,
        prompt: &'a str,
        subagent_depth: u32,
        emit_deltas: bool,
        subagent_type: &'a SubagentType,
    ) -> Self {
        Self {
            turn_id: TurnId::new(),
            cwd,
            prompt,
            subagent_depth,
            emit_deltas,
            subagent_type,
            root_task_id: None,
            continuation: None,
            steer_handle: None,
            provider_suspension_control: None,
            provider_response_ingress: None,
            workflow_lifecycle_ingress: None,
            wait_for_background_workflows: true,
            defer_cancel_terminal: false,
            permission_handler_owned: None,
        }
    }

    pub(crate) fn with_turn_id(mut self, turn_id: TurnId) -> Self {
        self.turn_id = turn_id;
        self
    }

    pub(crate) fn with_owned_permission_handler(
        mut self,
        handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    ) -> Self {
        self.permission_handler_owned = handler;
        self
    }

    pub(crate) fn permission_handler_owned(
        &self,
    ) -> Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>> {
        self.permission_handler_owned.clone()
    }

    pub(crate) fn with_root_task_id(mut self, root_task_id: Option<&'a str>) -> Self {
        self.root_task_id = root_task_id;
        self
    }

    pub(crate) fn with_continuation(mut self, continuation: RuntimeTurnContinuation) -> Self {
        self.continuation = Some(continuation);
        self
    }

    pub(crate) fn with_steer_handle(mut self, steer_handle: Option<&'a ThreadSteerHandle>) -> Self {
        self.steer_handle = steer_handle;
        self
    }

    pub(crate) fn with_provider_suspension_control(
        mut self,
        control: Option<&'a dyn RuntimeProviderSuspensionControl>,
    ) -> Self {
        self.provider_suspension_control = control;
        self
    }

    pub(crate) fn with_provider_response_ingress(
        mut self,
        ingress: Option<&'a dyn RuntimeProviderResponseIngress>,
    ) -> Self {
        self.provider_response_ingress = ingress;
        self
    }

    pub(crate) fn provider_response_ingress(
        &self,
    ) -> Option<&'a dyn RuntimeProviderResponseIngress> {
        self.provider_response_ingress
    }

    pub(crate) fn with_workflow_lifecycle_ingress(
        mut self,
        ingress: Option<&'a dyn RuntimeWorkflowLifecycleIngress>,
    ) -> Self {
        self.workflow_lifecycle_ingress = ingress;
        self
    }

    pub(crate) fn workflow_lifecycle_ingress(
        &self,
    ) -> Option<&'a dyn RuntimeWorkflowLifecycleIngress> {
        self.workflow_lifecycle_ingress
    }

    pub(crate) fn with_wait_for_background_workflows(mut self, wait: bool) -> Self {
        self.wait_for_background_workflows = wait;
        self
    }

    pub(crate) fn with_deferred_cancel_terminal(mut self, defer: bool) -> Self {
        self.defer_cancel_terminal = defer;
        self
    }

    #[cfg(test)]
    pub(crate) fn continuation(&self) -> Option<&RuntimeTurnContinuation> {
        self.continuation.as_ref()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn steer_handle(&self) -> Option<&'a ThreadSteerHandle> {
        self.steer_handle
    }

    #[cfg(test)]
    pub(crate) fn initial_response(&self) -> Option<&ProviderResponse> {
        self.continuation
            .as_ref()
            .map(|continuation| &continuation.response.response)
    }

    #[cfg(test)]
    pub(crate) fn cwd(&self) -> &'a Path {
        self.cwd
    }

    #[cfg(test)]
    pub(crate) fn prompt(&self) -> &'a str {
        self.prompt
    }

    #[cfg(test)]
    pub(crate) fn subagent_depth(&self) -> u32 {
        self.subagent_depth
    }

    #[cfg(test)]
    pub(crate) fn emit_deltas(&self) -> bool {
        self.emit_deltas
    }

    #[cfg(test)]
    pub(crate) fn subagent_type(&self) -> &'a SubagentType {
        self.subagent_type
    }
}

impl<'a> RuntimeTurnDeps<'a> {
    pub(crate) fn new(
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
    ) -> Self {
        Self {
            instructions,
            memory,
            mcp_registry,
            hooks,
            turn_interactions: RuntimeTurnInteractionState::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn instructions(&self) -> &'a ProjectInstructions {
        self.instructions
    }

    #[cfg(test)]
    pub(crate) fn memory(&self) -> &'a MemoryBlock {
        self.memory
    }

    #[cfg(test)]
    pub(crate) fn mcp_registry(&self) -> &'a McpRegistry {
        self.mcp_registry
    }

    #[cfg(test)]
    pub(crate) fn hooks(&self) -> &'a HookRunner {
        self.hooks
    }

    pub(crate) fn with_permission_handler(
        mut self,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    ) -> Self {
        self.turn_interactions = self
            .turn_interactions
            .with_permission_handler(permission_handler);
        self
    }

    pub(crate) fn with_approval_handler(
        mut self,
        approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
    ) -> Self {
        self.turn_interactions = self
            .turn_interactions
            .with_approval_handler(approval_handler);
        self
    }

    pub(crate) fn with_user_input_handler(
        mut self,
        user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    ) -> Self {
        self.turn_interactions = self
            .turn_interactions
            .with_user_input_handler(user_input_handler);
        self
    }

    pub(crate) fn with_mcp_elicitation_handler(
        mut self,
        mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    ) -> Self {
        self.turn_interactions = self
            .turn_interactions
            .with_mcp_elicitation_handler(mcp_elicitation_handler);
        self
    }

    #[cfg(test)]
    pub(crate) fn turn_interactions(&self) -> RuntimeTurnInteractionState<'a> {
        self.turn_interactions
    }
}

impl<'a> RuntimeTurnInteractionState<'a> {
    pub(crate) fn new() -> Self {
        Self {
            approval_handler: None,
            permission_handler: None,
            user_input_handler: None,
            mcp_elicitation_handler: None,
        }
    }

    pub(crate) fn with_approval_handler(
        mut self,
        approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
    ) -> Self {
        self.approval_handler = approval_handler;
        self
    }

    pub(crate) fn approval_handler(
        &self,
    ) -> Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)> {
        self.approval_handler
    }

    pub(crate) fn with_permission_handler(
        mut self,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    ) -> Self {
        self.permission_handler = permission_handler;
        self
    }

    pub(crate) fn permission_handler(
        &self,
    ) -> Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)> {
        self.permission_handler
    }

    pub(crate) fn with_user_input_handler(
        mut self,
        user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    ) -> Self {
        self.user_input_handler = user_input_handler;
        self
    }

    pub(crate) fn user_input_handler(&self) -> Option<&'a dyn RuntimeUserInputHandler> {
        self.user_input_handler
    }

    pub(crate) fn with_mcp_elicitation_handler(
        mut self,
        mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    ) -> Self {
        self.mcp_elicitation_handler = mcp_elicitation_handler;
        self
    }

    pub(crate) fn mcp_elicitation_handler(
        &self,
    ) -> Option<&'a (dyn McpElicitationHandler + Send + Sync)> {
        self.mcp_elicitation_handler
    }
}

impl<'a> RuntimeTurnState<'a> {
    pub(crate) fn new(
        cost_tracker: &'a mut CostTracker,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
    ) -> Self {
        Self::new_with_thread_extensions(
            cost_tracker,
            cancel,
            task_registry,
            Arc::new(ExtensionData::new(task_registry.session_id())),
            task_registry.session_id(),
        )
    }

    pub(crate) fn new_with_thread_extensions(
        cost_tracker: &'a mut CostTracker,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
        thread_extensions: Arc<ExtensionData>,
        turn_extension_id: impl Into<String>,
    ) -> Self {
        let extension_builder = ExtensionRegistryBuilder::new();
        Self {
            cost_tracker,
            cancel,
            task_registry,
            directive_state: RuntimeDirectiveState::default(),
            extension_registry: extension_builder.build(),
            thread_extensions,
            turn_extensions: Arc::new(ExtensionData::new(turn_extension_id)),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn apply_directive(&mut self, directive: RuntimeDirective) {
        RuntimeTurnReducer::new(self.thread_extensions.as_ref(), &self.turn_extensions)
            .apply_directive(&mut self.directive_state, directive);
    }

    #[allow(dead_code)]
    pub(crate) fn extension_context(&self) -> RuntimeExtensionContext<'_> {
        Self::extension_context_from_parts(
            &self.extension_registry,
            self.thread_extensions.as_ref(),
            &self.turn_extensions,
        )
    }

    pub(crate) fn extension_context_from_parts<'extensions>(
        extension_registry: &'extensions ExtensionRegistry,
        thread_extensions: &'extensions ExtensionData,
        turn_extensions: &'extensions ExtensionData,
    ) -> RuntimeExtensionContext<'extensions> {
        RuntimeExtensionContext::new(
            extension_registry,
            RuntimeExtensionStores::new(thread_extensions, turn_extensions),
        )
    }

    pub(crate) fn into_loop_state(self) -> RuntimeTurnLoopState<'a> {
        let kernel = RuntimeTurnKernel::new(
            self.thread_extensions.as_ref(),
            self.turn_extensions.as_ref(),
        );
        kernel.turn_loop_state(
            self.directive_state,
            self.cost_tracker,
            self.cancel,
            self.task_registry,
            self.extension_registry,
            Arc::clone(&self.thread_extensions),
            Arc::clone(&self.turn_extensions),
        )
    }

    #[cfg(test)]
    pub(crate) fn cost_tracker(&self) -> &CostTracker {
        self.cost_tracker
    }

    #[cfg(test)]
    pub(crate) fn cancel(&self) -> &'a CancelToken {
        self.cancel
    }

    #[cfg(test)]
    pub(crate) fn task_registry(&self) -> &'a TaskRegistry {
        self.task_registry
    }

    #[cfg(test)]
    pub(crate) fn extension_registry(&self) -> &ExtensionRegistry {
        &self.extension_registry
    }

    #[cfg(test)]
    pub(crate) fn thread_extensions(&self) -> &ExtensionData {
        self.thread_extensions.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn turn_extensions(&self) -> &ExtensionData {
        &self.turn_extensions
    }
}

impl RuntimeTurnExtensionState {
    pub(crate) fn new(
        extension_registry: ExtensionRegistry,
        thread_extensions: Arc<ExtensionData>,
        turn_extensions: Arc<ExtensionData>,
    ) -> Self {
        Self {
            extension_registry,
            thread_extensions,
            turn_extensions,
        }
    }

    pub(crate) fn extension_context(&self) -> RuntimeExtensionContext<'_> {
        RuntimeTurnState::extension_context_from_parts(
            &self.extension_registry,
            self.thread_extensions.as_ref(),
            &self.turn_extensions,
        )
    }
}

impl<'a> RuntimeTurnLoopState<'a> {
    pub(crate) fn tool_policy<'state>(
        &'state self,
        tool_policy: AgentToolPolicyContext<'state>,
    ) -> AgentToolPolicyContext<'state> {
        tool_policy.replace_allowed_tools(
            self.directive_state.allowed_tools(),
            "runtime directive tool policy",
        )
    }

    pub(crate) fn iteration_state<'state>(
        &'state mut self,
        tool_policy: AgentToolPolicyContext<'state>,
    ) -> RuntimeTurnLoopIterationState<'state> {
        let directive_state = &self.directive_state;
        let runtime = &mut self.runtime;
        RuntimeTurnLoopIterationState {
            runtime_system_messages: directive_state.pending_system_messages(),
            model_override: directive_state.model_override(),
            tool_policy: tool_policy.replace_allowed_tools(
                directive_state.allowed_tools(),
                "runtime directive tool policy",
            ),
            cost_tracker: &mut *runtime.cost_tracker,
            cancel: runtime.cancel,
            task_registry: runtime.task_registry,
            extensions: runtime.extensions.extension_context(),
        }
    }
}

impl<'a> RuntimeTurnExecution<'a> {
    pub(crate) fn new(
        background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
    ) -> Self {
        Self {
            background_workflows,
            workflow_ipc,
            lifecycle,
        }
    }

    #[cfg(test)]
    pub(crate) fn background_workflow_count(&self) -> usize {
        self.background_workflows.len()
    }

    #[cfg(test)]
    pub(crate) fn workflow_ipc(&self) -> Option<&'a WorkflowIpcContext> {
        self.workflow_ipc
    }

    #[cfg(test)]
    pub(crate) fn lifecycle(&self) -> Option<&RuntimeSessionLifecycle> {
        self.lifecycle.as_deref()
    }
}

fn attach_shell_task_to_tool_event(
    event: EventDraft,
    request: &ToolRequest,
    status: RuntimeTaskStatus,
) -> EventDraft {
    if request.action != orca_core::approval_types::ActionKind::Shell {
        return event;
    }

    RuntimeTaskLifecycle::new_snapshot(shell_task_id(request), RuntimeTaskKind::Shell, status, 1)
        .attach_to_event(event)
}

fn shell_task_id(request: &ToolRequest) -> String {
    format!("shell-{}:task-1", request.id)
}

impl RuntimeActorStartedTurn {
    pub fn turn(&self) -> u32 {
        self.turn
    }

    pub fn task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.task.as_ref()
    }

    pub fn event(&self) -> Option<&EventDraft> {
        self.event.as_ref()
    }

    pub fn into_event(self) -> Option<EventDraft> {
        self.event
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_conversation_bootstrap::{
        AgentConversationContext, RuntimeConversationBootstrapStep,
    };
    use crate::runtime_model_route::{RuntimeModelRouteInput, RuntimeModelRouteStep};
    use crate::runtime_turn_opening::{
        RuntimeTurnOpeningInput, RuntimeTurnOpeningResult, RuntimeTurnOpeningStep,
    };
    use crate::runtime_turn_setup::RuntimeTurnSetupStep;
    use crate::runtime_turn_start::{
        RuntimeTurnStartInput, RuntimeTurnStartResult, RuntimeTurnStartResultStep,
        RuntimeTurnStartStep, RuntimeTurnStartStepOutput,
    };
    use crate::tool_invocation::AgentToolPolicyContext;
    use orca_provider::context;

    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Message;
    use orca_core::event_sink::EventSink;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            mcp_servers: Vec::<McpServerConfig>::new(),
            external_tools: Vec::new(),
            hooks: Vec::<HookConfig>::new(),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn agent_loop_result_constructors_preserve_terminal_shape() {
        let success = AgentLoopResult::success(Some("done".to_string()));
        assert_eq!(success.status, RunStatus::Success);
        assert_eq!(success.reason, TurnEndReason::Unclassified);
        assert_eq!(success.final_message.as_deref(), Some("done"));
        assert_eq!(success.error, None);

        let failure = AgentLoopResult::failure(RunStatus::Failed, "provider failed");
        assert_eq!(failure.status, RunStatus::Failed);
        assert_eq!(failure.reason, TurnEndReason::Unclassified);
        assert_eq!(failure.final_message, None);
        assert_eq!(failure.error.as_deref(), Some("provider failed"));

        let terminal =
            AgentLoopResult::terminal(RunStatus::Cancelled, TurnEndReason::Cancelled, None);
        assert_eq!(terminal.status, RunStatus::Cancelled);
        assert_eq!(terminal.reason, TurnEndReason::Cancelled);
        assert_eq!(terminal.final_message, None);
        assert_eq!(terminal.error, None);
    }

    #[test]
    fn agent_loop_result_preserves_turn_end_reason() {
        let from_start = AgentLoopResult::from(RuntimeTurnStartError {
            status: RunStatus::Failed,
            reason: TurnEndReason::Unclassified,
            message: "provider failed".to_string(),
            max_cost_usd_micros: 0,
            terminal: None,
        });
        assert_eq!(from_start.status, RunStatus::Failed);
        assert_eq!(from_start.reason, TurnEndReason::Unclassified);
        assert_eq!(from_start.error.as_deref(), Some("provider failed"));

        let cost = AgentLoopResult::from(RuntimeTurnStartError {
            status: RunStatus::Failed,
            reason: TurnEndReason::CostBudgetExhausted,
            message: "budget stopped".to_string(),
            max_cost_usd_micros: 0,
            terminal: None,
        });
        assert_eq!(cost.reason, TurnEndReason::CostBudgetExhausted);
        assert_ne!(from_start.reason, cost.reason);
    }

    #[test]
    fn tool_terminal_events_preserve_cancelled_and_unknown_task_statuses() {
        let request = ToolRequest {
            id: "call-1".to_string(),
            name: orca_core::tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("sleep 30".to_string()),
            raw_arguments: None,
        };
        let mut events = EventFactory::new("tool-terminal-status".to_string());

        let cancelled = ToolResult::cancelled(&request, "turn interrupted", Some(130));
        let cancelled_event =
            RuntimeTaskActor::tool_call_completed_event_for(&mut events, &request, &cancelled);
        assert_eq!(cancelled_event.payload["status"], "cancelled");
        assert_eq!(cancelled_event.payload["task"]["status"], "cancelled");

        let indeterminate = ToolResult::indeterminate(&request, "missing terminal result");
        let indeterminate_event =
            RuntimeTaskActor::tool_call_completed_event_for(&mut events, &request, &indeterminate);
        assert_eq!(indeterminate_event.payload["status"], "indeterminate");
        assert_eq!(indeterminate_event.payload["task"]["status"], "failed");
    }

    #[test]
    fn turn_start_result_step_folds_error_and_continue() {
        let continuing =
            RuntimeTurnStartResultStep::new().fold(RuntimeTurnStartStepOutput { error: None });
        assert!(matches!(continuing, RuntimeTurnStartResult::Continue));

        let failed = RuntimeTurnStartResultStep::new().fold(RuntimeTurnStartStepOutput {
            error: Some(RuntimeTurnStartError {
                status: RunStatus::Failed,
                reason: TurnEndReason::Unclassified,
                message: "max turns exceeded".to_string(),
                max_cost_usd_micros: 0,
                terminal: None,
            }),
        });
        match failed {
            RuntimeTurnStartResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert_eq!(result.final_message, None);
                assert_eq!(result.error.as_deref(), Some("max turns exceeded"));
            }
            RuntimeTurnStartResult::Continue => {
                panic!("turn-start error should return loop result")
            }
        }
    }

    #[test]
    fn turn_start_step_emits_started_event() {
        let mut lifecycle = RuntimeSessionLifecycle::new("turn-start-step".to_string());
        let mut actor = RuntimeTaskActor::new(&mut lifecycle);
        let mut events = EventFactory::new("turn-start-step".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let subagent_type = SubagentType::General;
        let cwd = Path::new(".");
        let turn_context = RuntimeTurnContext::new(cwd, "hello", 3, true, &subagent_type);

        let result = RuntimeTurnStartStep::new()
            .start(RuntimeTurnStartInput {
                actor: &mut actor,
                events: &mut events,
                sink: &mut sink,
                turn_context,
            })
            .expect("start turn");

        assert!(result.error.is_none());
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"turn.started\""));
        assert!(output.contains("hello"));
    }

    #[test]
    fn turn_opening_step_compacts_starts_routes_and_steers() {
        let mut lifecycle = RuntimeSessionLifecycle::new("turn-opening-step".to_string());
        let mut actor = RuntimeTaskActor::new(&mut lifecycle);
        let mut events = EventFactory::new("turn-opening-step".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let provider_config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &runtime,
        );
        let hooks = HookRunner::default();
        let mut conversation = Conversation::new();
        let mut cost_tracker = CostTracker::new(None);
        let model = ModelSelection::parse(None).expect("model");
        let subagent_type = SubagentType::General;
        let cwd = Path::new(".");
        let turn_context = RuntimeTurnContext::new(cwd, "hello", 3, true, &subagent_type);

        let result = RuntimeTurnOpeningStep::new()
            .open(RuntimeTurnOpeningInput {
                actor: &mut actor,
                operation: &mut crate::operation_context::OperationContext::for_tests(
                    orca_core::budget::BudgetSpec::default(),
                    "turn-opening-step",
                ),
                provider: ProviderKind::DeepSeek,
                context_config: &context_config,
                provider_config: &provider_config,
                turn_context,
                hooks: &hooks,
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                model: &model,
                model_override: None,
                cost_tracker: &mut cost_tracker,
            })
            .expect("open turn");

        match result {
            RuntimeTurnOpeningResult::Continue {
                provider_config, ..
            } => {
                assert_eq!(provider_config.api_key.as_deref(), Some("test-key"));
                assert_eq!(
                    provider_config.model.as_deref(),
                    Some(orca_core::model::PRO_MODEL)
                );
            }
            RuntimeTurnOpeningResult::Return(_) => panic!("opening should continue"),
        }
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"turn.started\""));
        assert!(output.contains("\"type\":\"model.routed\""));
    }

    #[test]
    fn turn_opening_injects_pending_cost_soft_landing_once() {
        let mut lifecycle = RuntimeSessionLifecycle::new("cost-soft-landing".to_string());
        let mut actor = RuntimeTaskActor::new(&mut lifecycle);
        let mut cost_tracker = CostTracker::new(Some("deepseek-v4-flash"));
        actor
            .record_usage(
                Usage {
                    input_tokens: 600_000,
                    output_tokens: 0,
                    cache_tokens: 0,
                },
                &mut cost_tracker,
                Some(100_000),
            )
            .expect("usage remains below hard cost wall");

        let mut events = EventFactory::new("cost-soft-landing".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let provider_config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some(orca_core::model::FLASH_MODEL),
            &runtime,
        );
        let hooks = HookRunner::default();
        let mut conversation = Conversation::new();
        let model = ModelSelection::parse(None).expect("model");
        let subagent_type = SubagentType::General;
        let turn_context =
            RuntimeTurnContext::new(Path::new("."), "continue", 0, false, &subagent_type);

        let result = RuntimeTurnOpeningStep::new()
            .open(RuntimeTurnOpeningInput {
                actor: &mut actor,
                operation: &mut crate::operation_context::OperationContext::for_tests(
                    orca_core::budget::BudgetSpec::default(),
                    "turn-opening-step",
                ),
                provider: ProviderKind::DeepSeek,
                context_config: &context_config,
                provider_config: &provider_config,
                turn_context,
                hooks: &hooks,
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                model: &model,
                model_override: None,
                cost_tracker: &mut cost_tracker,
            })
            .expect("open turn");
        assert!(matches!(result, RuntimeTurnOpeningResult::Continue { .. }));
        assert!(conversation.messages.iter().any(|message| {
            matches!(
                message,
                Message::System { content, pinned: true }
                    if content.contains("Cost budget is low")
            )
        }));
        assert!(
            actor.take_pending_cost_budget_soft_landing().is_none(),
            "the same crossed cost threshold must not be delivered twice"
        );
    }

    #[test]
    fn model_route_step_returns_provider_config_and_emits_event() {
        let mut lifecycle = RuntimeSessionLifecycle::new("model-route-step".to_string());
        let mut actor = RuntimeTaskActor::new(&mut lifecycle);
        let mut events = EventFactory::new("model-route-step".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let provider_config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let mut cost_tracker = CostTracker::new(None);
        let model = ModelSelection::parse(None).expect("model");
        let subagent_type = SubagentType::General;
        let cwd = Path::new(".");
        let turn_context = RuntimeTurnContext::new(cwd, "", 3, true, &subagent_type);

        let result = RuntimeModelRouteStep::new()
            .route(RuntimeModelRouteInput {
                actor: &mut actor,
                model: &model,
                turn_context,
                has_images: false,
                model_override: None,
                provider_config: &provider_config,
                cost_tracker: &mut cost_tracker,
                events: &mut events,
                sink: &mut sink,
            })
            .expect("route model");

        assert_eq!(result.provider_config.api_key.as_deref(), Some("test-key"));
        assert_eq!(
            result.provider_config.model.as_deref(),
            Some(orca_core::model::PRO_MODEL)
        );
        assert_eq!(result.decision.actual_model, orca_core::model::PRO_MODEL);
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"model.routed\""));
        assert!(output.contains(orca_core::model::PRO_MODEL));
    }

    #[test]
    fn model_route_step_applies_runtime_model_override_directive() {
        let mut lifecycle = RuntimeSessionLifecycle::new("model-route-directive".to_string());
        let mut actor = RuntimeTaskActor::new(&mut lifecycle);
        let mut events = EventFactory::new("model-route-directive".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let provider_config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let mut cost_tracker = CostTracker::new(None);
        let model = ModelSelection::parse(None).expect("model");
        let subagent_type = SubagentType::General;
        let cwd = Path::new(".");
        let turn_context = RuntimeTurnContext::new(cwd, "", 3, true, &subagent_type);
        let mut directive_state = RuntimeDirectiveState::default();
        directive_state.apply(RuntimeDirective::SwitchModel {
            model: orca_core::model::FLASH_MODEL.to_string(),
            reason: "runtime policy selected flash".to_string(),
        });

        let result = RuntimeModelRouteStep::new()
            .route(RuntimeModelRouteInput {
                actor: &mut actor,
                model: &model,
                turn_context,
                has_images: false,
                model_override: directive_state.model_override(),
                provider_config: &provider_config,
                cost_tracker: &mut cost_tracker,
                events: &mut events,
                sink: &mut sink,
            })
            .expect("route model");

        assert_eq!(
            result.provider_config.model.as_deref(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert_eq!(result.decision.actual_model, orca_core::model::FLASH_MODEL);
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains(orca_core::model::FLASH_MODEL));
    }

    #[test]
    fn turn_setup_step_builds_runtime_context_policy_and_provider_config() {
        let mut config = config();
        config.api_key = Some("test-key".to_string());
        config.model =
            ModelSelection::parse(Some(orca_core::model::FLASH_MODEL.to_string())).expect("model");
        config.model_runtime = ModelRuntimeConfig {
            context_window: Some(128_000),
            auto_compact_token_limit: Some(96_000),
            soft_compact_token_limit: None,
        };
        let mcp_registry = McpRegistry::default();

        let setup = RuntimeTurnSetupStep::new().prepare(
            &config,
            0,
            &SubagentType::General,
            AgentToolPolicyContext::unrestricted(),
            &mcp_registry,
        );

        assert_eq!(setup.context_config.max_tokens, 128_000);
        assert_eq!(setup.context_config.effective_limit(), 96_000);
        assert_eq!(setup.provider_config.api_key.as_deref(), Some("test-key"));
        assert_eq!(
            setup.provider_config.model.as_deref(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert!(setup.provider_config.mcp_registry.is_some());
    }

    #[test]
    fn conversation_bootstrap_step_builds_owned_conversation_when_missing() {
        let config = config();
        let cwd = tempfile::tempdir().expect("cwd");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let context = AgentConversationContext::owned();

        let mut prepared = RuntimeConversationBootstrapStep::new().prepare(
            context,
            cwd.path(),
            "inspect repo",
            0,
            &SubagentType::General,
            &instructions,
            config.approval_mode,
            &memory,
        );
        let conversation = prepared.conversation_mut();

        assert_eq!(conversation.messages.len(), 2);
        assert!(
            matches!(&conversation.messages[0], Message::System { .. }),
            "owned bootstrap should seed a system prompt"
        );
        assert!(
            matches!(&conversation.messages[1], Message::User { content, .. } if content == "inspect repo"),
            "owned bootstrap should seed the user prompt"
        );
        assert!(prepared.history_writer_mut().is_none());
    }

    #[test]
    fn budget_stops_carry_distinct_reasons() {
        let failed = RuntimeTurnStartError {
            status: RunStatus::Failed,
            reason: TurnEndReason::Unclassified,
            message: "provider failed".to_string(),
            max_cost_usd_micros: 0,
            terminal: None,
        };
        let cost = RuntimeTurnStartError {
            status: RunStatus::Failed,
            reason: TurnEndReason::CostBudgetExhausted,
            message: "budget stopped".to_string(),
            max_cost_usd_micros: 0,
            terminal: None,
        };

        assert_eq!(failed.status, cost.status);
        assert_ne!(failed.reason, cost.reason);
        assert_eq!(TurnEndReason::default(), TurnEndReason::Unclassified);
    }
}
