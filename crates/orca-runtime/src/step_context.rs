use std::io;
#[cfg(test)]
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::RunStatus;
use orca_core::tool_types::{ToolRequest, ToolResult};
use orca_mcp::{McpElicitationHandler, McpRegistry};

use crate::extension::RuntimeExtensionContext;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimeApprovalHandler, RuntimePermissionRequestHandler, RuntimeTurnContext,
    RuntimeUserInputHandler, TurnPermissionOverlay,
};
use crate::memory::MemoryBlock;
use crate::session::{record_plan_state_for_agent, record_tool_result_for_agent};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::ipc::WorkflowIpcContext;

#[derive(Clone, Copy)]
pub(crate) struct RuntimeStepCapabilitySnapshot<'a> {
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    pub(crate) user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    pub(crate) mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
}

#[derive(Clone)]
pub(crate) struct RuntimeStepSnapshot<'a> {
    pub(crate) config: &'a RunConfig,
    pub(crate) turn_context: RuntimeTurnContext<'a>,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) capabilities: RuntimeStepCapabilitySnapshot<'a>,
}

#[derive(Clone)]
pub(crate) struct RuntimeStepContext<'a> {
    pub(crate) snapshot: RuntimeStepSnapshot<'a>,
    pub(crate) extensions: Option<RuntimeExtensionContext<'a>>,
}

#[derive(Default)]
pub(crate) struct RuntimeSamplingRequestState {
    pub(crate) permission_overlay: TurnPermissionOverlay,
    tool_cursor_index: usize,
}

pub(crate) struct RuntimeToolDispatchWindow<'a> {
    tool_requests: &'a [ToolRequest],
    end_index: usize,
}

pub(crate) enum RuntimeToolResultRecordOutcome {
    Continue,
    Return {
        status: RunStatus,
        error: Option<String>,
    },
}

impl<'a> RuntimeToolDispatchWindow<'a> {
    pub(crate) fn tool_requests(&self) -> &'a [ToolRequest] {
        self.tool_requests
    }

    pub(crate) fn end_index(&self) -> usize {
        self.end_index
    }
}

impl RuntimeSamplingRequestState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn permission_overlay_mut(&mut self) -> &mut TurnPermissionOverlay {
        &mut self.permission_overlay
    }

    pub(crate) fn current_tool_request<'a>(
        &self,
        tool_requests: &'a [ToolRequest],
    ) -> Option<&'a ToolRequest> {
        tool_requests.get(self.tool_cursor_index)
    }

    #[cfg(test)]
    pub(crate) fn tool_cursor_position(&self) -> usize {
        self.tool_cursor_index
    }

    pub(crate) fn advance_tool_cursor_one(&mut self, tool_request_count: usize) {
        self.advance_tool_cursor_to(self.tool_cursor_index.saturating_add(1), tool_request_count);
    }

    pub(crate) fn advance_tool_cursor_to(&mut self, next_index: usize, tool_request_count: usize) {
        self.tool_cursor_index = next_index.min(tool_request_count);
    }

    pub(crate) fn tool_dispatch_window<'a, F>(
        &self,
        tool_requests: &'a [ToolRequest],
        collect_end: F,
    ) -> RuntimeToolDispatchWindow<'a>
    where
        F: FnOnce(&[ToolRequest], usize) -> usize,
    {
        let start_index = self.tool_cursor_index.min(tool_requests.len());
        let minimum_end_index = start_index.saturating_add(1).min(tool_requests.len());
        let end_index = collect_end(tool_requests, start_index)
            .min(tool_requests.len())
            .max(minimum_end_index);
        RuntimeToolDispatchWindow {
            tool_requests: &tool_requests[start_index..end_index],
            end_index,
        }
    }

    pub(crate) fn advance_tool_cursor_to_window_end(
        &mut self,
        window: &RuntimeToolDispatchWindow<'_>,
    ) {
        self.tool_cursor_index = window.end_index();
    }

    pub(crate) fn record_normal_tool_result(
        &self,
        conversation: &mut Conversation,
        mut history_writer: Option<&mut SessionWriter>,
        tool_request: &ToolRequest,
        result: &ToolResult,
        status: RunStatus,
        emit_deltas: bool,
    ) -> io::Result<RuntimeToolResultRecordOutcome> {
        record_plan_state_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            tool_request,
            result,
        );
        record_tool_result_for_agent(conversation, history_writer, result, emit_deltas)?;

        let terminal_continuation_violation = tool_request.name
            == orca_core::tool_types::ToolName::Subagent
            && result
                .error
                .as_deref()
                .is_some_and(is_terminal_continuation_violation);
        if matches!(status, RunStatus::ApprovalRequired | RunStatus::Cancelled)
            || result.status == orca_core::tool_types::ToolStatus::Indeterminate
            || terminal_continuation_violation
        {
            return Ok(RuntimeToolResultRecordOutcome::Return {
                status,
                error: result.error.clone(),
            });
        }
        Ok(RuntimeToolResultRecordOutcome::Continue)
    }
}

pub(crate) fn is_terminal_continuation_violation(error: &str) -> bool {
    error.contains("continuation_parent_mismatch")
        || error.contains("continuation parent mismatch")
        || error.contains("continuation task binding mismatch")
        || error.contains("continuation fence mismatch")
        || error.contains("attempt fence mismatch")
        || error.contains("continuation lease epoch mismatch")
}

impl<'a> RuntimeStepSnapshot<'a> {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: &'a RunConfig,
        cwd: &'a Path,
        tool_policy: AgentToolPolicyContext<'a>,
        subagent_depth: u32,
        emit_deltas: bool,
        policy: &'a ApprovalPolicy,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
        user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
        mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    ) -> Self {
        static GENERAL_SUBAGENT_TYPE: orca_core::subagent_types::SubagentType =
            orca_core::subagent_types::SubagentType::General;
        Self::new_with_capabilities(
            config,
            RuntimeTurnContext::new(cwd, "", subagent_depth, emit_deltas, &GENERAL_SUBAGENT_TYPE),
            tool_policy,
            policy,
            RuntimeStepCapabilitySnapshot::new(
                instructions,
                memory,
                mcp_registry,
                hooks,
                cancel,
                task_registry,
                workflow_ipc,
                approval_handler,
                permission_handler,
                user_input_handler,
                mcp_elicitation_handler,
            ),
        )
    }

    pub(crate) fn new_with_capabilities(
        config: &'a RunConfig,
        turn_context: RuntimeTurnContext<'a>,
        tool_policy: AgentToolPolicyContext<'a>,
        policy: &'a ApprovalPolicy,
        capabilities: RuntimeStepCapabilitySnapshot<'a>,
    ) -> Self {
        Self {
            config,
            turn_context,
            tool_policy,
            policy,
            capabilities,
        }
    }

    pub(crate) fn capabilities(&self) -> RuntimeStepCapabilitySnapshot<'a> {
        self.capabilities
    }
}

impl<'a> RuntimeStepCapabilitySnapshot<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
        user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
        mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    ) -> Self {
        Self {
            instructions,
            memory,
            mcp_registry,
            hooks,
            cancel,
            task_registry,
            workflow_ipc,
            approval_handler,
            permission_handler,
            user_input_handler,
            mcp_elicitation_handler,
        }
    }
}

impl<'a> RuntimeStepContext<'a> {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: &'a RunConfig,
        cwd: &'a Path,
        tool_policy: AgentToolPolicyContext<'a>,
        subagent_depth: u32,
        emit_deltas: bool,
        policy: &'a ApprovalPolicy,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
        cancel: &'a CancelToken,
        task_registry: &'a TaskRegistry,
        workflow_ipc: Option<&'a WorkflowIpcContext>,
        approval_handler: Option<&'a (dyn RuntimeApprovalHandler + Send + Sync)>,
        permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
        user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
        mcp_elicitation_handler: Option<&'a (dyn McpElicitationHandler + Send + Sync)>,
    ) -> Self {
        Self {
            snapshot: RuntimeStepSnapshot::new(
                config,
                cwd,
                tool_policy,
                subagent_depth,
                emit_deltas,
                policy,
                instructions,
                memory,
                mcp_registry,
                hooks,
                cancel,
                task_registry,
                workflow_ipc,
                approval_handler,
                permission_handler,
                user_input_handler,
                mcp_elicitation_handler,
            ),
            extensions: None,
        }
    }

    pub(crate) fn from_snapshot(snapshot: RuntimeStepSnapshot<'a>) -> Self {
        Self {
            snapshot,
            extensions: None,
        }
    }

    pub(crate) fn snapshot(&self) -> RuntimeStepSnapshot<'a> {
        self.snapshot.clone()
    }

    pub(crate) fn into_parts(
        self,
    ) -> (RuntimeStepSnapshot<'a>, Option<RuntimeExtensionContext<'a>>) {
        (self.snapshot, self.extensions)
    }
}
