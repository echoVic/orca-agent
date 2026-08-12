use std::io;
use std::path::{Path, PathBuf};

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalRequest, ApprovalResolution};
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

use crate::extension::ExtensionData;
use crate::lifecycle::{
    RuntimeApprovalDecision, RuntimeApprovalHandler, RuntimePermissionRequestHandler,
    RuntimeSessionLifecycle, RuntimeTaskActor, RuntimeTaskKind, RuntimeTaskLifecycle,
    RuntimeUserInputHandler, TurnPermissionOverlay,
};
use crate::runtime_state::PermissionRuntimeState;
use crate::runtime_tool_call::{
    RuntimeNormalToolInteractions, RuntimeNormalToolInvocation, RuntimeToolCallRuntime,
};
use crate::tasks::TaskRegistry;

pub struct RuntimeToolActorContext {
    lifecycle: RuntimeSessionLifecycle,
    pub(crate) permission_overlay: TurnPermissionOverlay,
    pub(crate) thread_extensions: ExtensionData,
    pub(crate) turn_extensions: ExtensionData,
}

impl RuntimeToolActorContext {
    pub fn new(run_id: impl Into<String>) -> Self {
        let run_id = run_id.into();
        let thread_extension_id = run_id.clone();
        let turn_extension_id = format!("{run_id}:tool-actor");
        let mut lifecycle = RuntimeSessionLifecycle::new(run_id);
        lifecycle.start_task(RuntimeTaskKind::Agent);
        Self {
            lifecycle,
            permission_overlay: TurnPermissionOverlay::default(),
            thread_extensions: ExtensionData::new(thread_extension_id),
            turn_extensions: ExtensionData::new(turn_extension_id),
        }
    }

    fn actor(&mut self) -> RuntimeTaskActor<'_> {
        RuntimeTaskActor::new(&mut self.lifecycle)
    }

    pub fn active_task(&self) -> Option<&RuntimeTaskLifecycle> {
        self.lifecycle.active_task()
    }

    pub fn granted_additional_working_directories(&self) -> Vec<PathBuf> {
        self.permission_overlay
            .additional_working_directories()
            .to_vec()
    }

    pub fn permission_overlay(&self) -> &TurnPermissionOverlay {
        &self.permission_overlay
    }

    pub fn run_pre_tool_hook(
        &mut self,
        hooks: &crate::hooks::HookRunner,
        cwd: &str,
        request: &ToolRequest,
    ) -> Result<crate::hooks::HookOutcome, ToolResult> {
        self.actor().run_pre_tool_hook(hooks, cwd, request)
    }

    pub fn run_pre_tool_hook_with_cancel(
        &mut self,
        hooks: &crate::hooks::HookRunner,
        cwd: &str,
        request: &ToolRequest,
        cancel: Option<&CancelToken>,
    ) -> Result<crate::hooks::HookOutcome, ToolResult> {
        self.actor()
            .run_pre_tool_hook_with_cancel(hooks, cwd, request, cancel)
    }

    pub fn run_post_tool_hook(
        &mut self,
        hooks: &crate::hooks::HookRunner,
        cwd: &str,
        request: &ToolRequest,
        result: &ToolResult,
    ) -> Option<String> {
        self.actor().run_post_tool_hook(hooks, cwd, request, result)
    }

    pub fn run_post_tool_hook_with_cancel(
        &mut self,
        hooks: &crate::hooks::HookRunner,
        cwd: &str,
        request: &ToolRequest,
        result: &ToolResult,
        cancel: Option<&CancelToken>,
    ) -> Option<String> {
        self.actor()
            .run_post_tool_hook_with_cancel(hooks, cwd, request, result, cancel)
    }

    pub fn resolve_tool_approval(
        &mut self,
        policy: &ApprovalPolicy,
        approval: Option<ApprovalRequest>,
        request: &ToolRequest,
    ) -> RuntimeApprovalDecision {
        self.actor()
            .resolve_tool_approval(policy, approval, request)
    }

    pub fn resolve_interactive_tool_approval(
        &mut self,
        handler: &dyn RuntimeApprovalHandler,
        approval: &ApprovalRequest,
        request: &ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        self.actor()
            .resolve_interactive_tool_approval(handler, approval, request)
    }

    pub fn execute_normal_tool(
        &mut self,
        request: &ToolRequest,
        cwd: &Path,
        mcp_registry: &McpRegistry,
        external_tools: &[ExternalToolConfig],
        output_truncation: ToolOutputTruncation,
        shell_timeout_secs: u64,
        task_registry: Option<&TaskRegistry>,
    ) -> ToolResult {
        self.execute_normal_tool_with_roots_and_cancel(
            None,
            request,
            cwd,
            &[],
            mcp_registry,
            external_tools,
            output_truncation,
            shell_timeout_secs,
            task_registry,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn execute_normal_tool_with_cancel(
        &mut self,
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
            None,
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
        config: Option<&RunConfig>,
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
            self.permission_overlay.clone(),
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
            Ok(output) => {
                PermissionRuntimeState
                    .merge_permission_delta(&mut self.permission_overlay, &output.permission_delta);
                output.result
            }
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
        self.actor().execute_user_input_tool(request, handler)
    }
}
