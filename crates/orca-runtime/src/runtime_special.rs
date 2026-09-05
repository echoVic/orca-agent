use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use orca_core::approval_types::ApprovalMode;
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::extension::RuntimeExtensionStores;
use crate::goal_actor::{GoalRuntimeHandle, GoalTurnContext};
use crate::goal_store::CreateGoalInput;
use crate::lifecycle::{
    AllowRequestedPermissions, RuntimePermissionRequest, RuntimePermissionRequestHandler,
    RuntimeSubagentStatusLookup, RuntimeToolActorContext, RuntimeUsageTotals, RuntimeWorkflowIpc,
};
use crate::protocol::{PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile};
use crate::runtime_permission::{RuntimePermissionPolicy, RuntimePermissionPromptDecision};
use crate::runtime_state::RuntimeTurnReducer;
use crate::tasks::TaskRegistry;
use crate::workflow::WorkflowDraftStore;

const DEFAULT_SUBAGENT_STATUS_PAGE_CHARS: usize = 12_000;
const MAX_SUBAGENT_STATUS_PAGE_CHARS: usize = 32_000;

fn goal_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimePermissionRequestArgs {
    #[serde(default)]
    reason: Option<String>,
    permissions: RequestPermissionProfile,
}

#[derive(Debug, Deserialize)]
struct SubagentStatusRequestArgs {
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

struct PagedAsyncSubagentResult {
    output: Option<Value>,
    task: Option<Value>,
    total_chars: Option<usize>,
    offset: Option<usize>,
    next_offset: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeSpecialToolDispatch {
    GetGoal,
    CreateGoal,
    UpdateGoal,
    WorkflowDraft,
    WorkflowDraftAction,
    Workflow,
    Subagent,
    SubagentStatus,
    TaskList,
    TaskStop,
    RequestPermissions,
    RequestUserInput,
    WorkflowIpc,
    Normal,
}

pub(crate) enum RuntimeGoalToolOutcome {
    Continue(ToolResult),
    StopTurn(ToolResult),
}

pub(crate) struct RuntimeGoalToolRequest<'a, W: io::Write> {
    pub(crate) persistent_session_id: Option<&'a str>,
    pub(crate) goal_runtime: Option<GoalRuntimeHandle>,
    pub(crate) goal_turn: Option<GoalTurnContext>,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) event_error: &'a mut Option<io::Error>,
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeWorkflowDraftRequest<'a> {
    pub workflows_enabled: bool,
    pub cwd: &'a Path,
    pub session_id: &'a str,
    pub max_concurrent_agents: usize,
}

impl RuntimeToolActorContext {
    pub fn classify_dispatch(
        &self,
        request: &ToolRequest,
        goal_mode: bool,
    ) -> RuntimeSpecialToolDispatch {
        match request.name {
            ToolName::GetGoal if goal_mode => RuntimeSpecialToolDispatch::GetGoal,
            ToolName::CreateGoal if goal_mode => RuntimeSpecialToolDispatch::CreateGoal,
            ToolName::UpdateGoal if goal_mode => RuntimeSpecialToolDispatch::UpdateGoal,
            ToolName::WorkflowDraft => RuntimeSpecialToolDispatch::WorkflowDraft,
            ToolName::WorkflowDraftAction => RuntimeSpecialToolDispatch::WorkflowDraftAction,
            ToolName::Workflow => RuntimeSpecialToolDispatch::Workflow,
            ToolName::Subagent => RuntimeSpecialToolDispatch::Subagent,
            ToolName::SubagentStatus => RuntimeSpecialToolDispatch::SubagentStatus,
            ToolName::TaskList => RuntimeSpecialToolDispatch::TaskList,
            ToolName::TaskStop => RuntimeSpecialToolDispatch::TaskStop,
            ToolName::RequestPermissions => RuntimeSpecialToolDispatch::RequestPermissions,
            ToolName::AskUserQuestion => RuntimeSpecialToolDispatch::RequestUserInput,
            ToolName::WorkflowSendMessage
            | ToolName::WorkflowReadMessages
            | ToolName::WorkflowClearMessages
            | ToolName::WorkflowCreateTaskList
            | ToolName::WorkflowClaimTask
            | ToolName::WorkflowCompleteTask
            | ToolName::WorkflowListTasks => RuntimeSpecialToolDispatch::WorkflowIpc,
            _ => RuntimeSpecialToolDispatch::Normal,
        }
    }

    pub(crate) fn execute_goal_tool<W: io::Write>(
        &mut self,
        request: &ToolRequest,
        context: RuntimeGoalToolRequest<'_, W>,
    ) -> RuntimeGoalToolOutcome {
        let Some(session_id) = context.persistent_session_id else {
            return RuntimeGoalToolOutcome::StopTurn(ToolResult::failed(
                request,
                "goal tools require a persistent session owned by the live runtime",
                None,
            ));
        };
        let Some(goal_runtime) = context.goal_runtime else {
            return RuntimeGoalToolOutcome::StopTurn(ToolResult::failed(
                request,
                "goal tools require a runtime-owned goal actor",
                None,
            ));
        };
        let result = match request.name {
            ToolName::GetGoal => match orca_tools::update_goal::parse_get_args(request) {
                Ok(()) => goal_runtime
                    .project_thread_goal(session_id)
                    .map(|goal| orca_tools::update_goal::completed_result(request, goal.as_ref())),
                Err(error) => Ok(ToolResult::failed(request, error, None)),
            },
            ToolName::CreateGoal => {
                let args = match orca_tools::update_goal::parse_create_args(request) {
                    Ok(args) => args,
                    Err(error) => {
                        return RuntimeGoalToolOutcome::Continue(ToolResult::failed(
                            request, error, None,
                        ));
                    }
                };
                match goal_runtime.read(session_id) {
                    Ok(Some(goal)) if goal.state.should_continue() => Ok(ToolResult::failed(
                        request,
                        "cannot create a goal because an active goal already exists",
                        None,
                    )),
                    Ok(Some(_)) => Ok(ToolResult::failed(
                        request,
                        "cannot create a goal until the existing goal is cleared",
                        None,
                    )),
                    Ok(None) => goal_runtime
                        .create(CreateGoalInput {
                            session_id: session_id.to_string(),
                            objective: args.objective,
                            token_budget: args.token_budget,
                            now: goal_now(),
                        })
                        .and_then(|record| {
                            let event = context.events.goal_created(&record);
                            if let Err(error) = context.sink.emit(event) {
                                *context.event_error = Some(error);
                            }
                            goal_runtime.project_thread_goal(session_id)
                        })
                        .map(|goal| {
                            orca_tools::update_goal::completed_result(request, goal.as_ref())
                        }),
                    Err(error) => Err(error),
                }
            }
            ToolName::UpdateGoal => {
                let intent = match orca_tools::update_goal::parse_update_intent(request) {
                    Ok(intent) => intent,
                    Err(error) => {
                        return RuntimeGoalToolOutcome::Continue(ToolResult::failed(
                            request, error, None,
                        ));
                    }
                };
                let Some(turn) = context.goal_turn.clone() else {
                    return RuntimeGoalToolOutcome::StopTurn(ToolResult::failed(
                        request,
                        "update_goal requires an active runtime outer turn",
                        None,
                    ));
                };
                let requested = context
                    .events
                    .goal_intent_requested(&turn.outer_turn_id, &intent);
                if let Err(error) = context.sink.emit(requested) {
                    *context.event_error = Some(error);
                }
                goal_runtime
                    .submit_intent(&turn.session_id, intent.clone(), goal_now())
                    .map(|ack| {
                        let acknowledged = context.events.goal_intent_acknowledged(
                            &turn.outer_turn_id,
                            &intent,
                            &ack,
                        );
                        if let Err(error) = context.sink.emit(acknowledged) {
                            *context.event_error = Some(error);
                        }
                        orca_tools::update_goal::acknowledgement_result(request, &ack)
                    })
            }
            _ => Ok(ToolResult::failed(
                request,
                "unsupported goal tool operation",
                None,
            )),
        };
        match result {
            Ok(result) => RuntimeGoalToolOutcome::Continue(result),
            Err(error) => RuntimeGoalToolOutcome::StopTurn(ToolResult::failed(
                request,
                format!("failed to access runtime-owned goal state: {error}"),
                None,
            )),
        }
    }

    pub fn execute_request_permissions_tool(&mut self, request: &ToolRequest) -> ToolResult {
        self.execute_request_permissions_tool_with_handler(request, &AllowRequestedPermissions)
    }

    pub fn execute_request_permissions_tool_with_handler(
        &mut self,
        request: &ToolRequest,
        handler: &dyn RuntimePermissionRequestHandler,
    ) -> ToolResult {
        let args = match parse_runtime_permission_request(request) {
            Ok(args) => args,
            Err(error) => return ToolResult::invalid_input(request, error),
        };
        let permission_request = RuntimePermissionRequest {
            id: request.id.clone(),
            reason: args.reason,
            permissions: args.permissions,
            context: crate::runtime_permission::RuntimePermissionContext::foreground(
                crate::surface::SurfacePermissionOrigin::SpecialTool,
            ),
        };
        let reducer = RuntimeTurnReducer::from_extension_stores(RuntimeExtensionStores::new(
            &self.thread_extensions,
            &self.turn_extensions,
        ));
        let response = match reducer.request_permission(
            &mut self.permission_overlay,
            handler,
            permission_request.clone(),
        ) {
            Ok(response) => response,
            Err(error) => return ToolResult::failed(request, error.to_string(), None),
        };
        if response.decision == PermissionResponseDecision::Deny {
            return ToolResult::denied(request, "permission request denied".to_string());
        }
        let write_roots = response
            .permissions
            .file_system
            .as_ref()
            .and_then(|file_system| file_system.write.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
            .collect::<Vec<_>>();
        let read_roots = response
            .permissions
            .file_system
            .as_ref()
            .and_then(|file_system| file_system.read.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let write_roots_json = write_roots
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let network_enabled = response
            .permissions
            .network
            .as_ref()
            .and_then(|network| network.enabled);
        let network_domains = response
            .permissions
            .network
            .as_ref()
            .map(|network| network.domains.clone())
            .unwrap_or_default();
        let output = json!({
            "message": "Permissions granted for the current turn",
            "reason": permission_request.reason,
            "granted": {
                "fileSystem": {
                    "read": read_roots,
                    "write": write_roots_json,
                },
                "network": {
                    "enabled": network_enabled,
                    "domains": network_domains,
                },
            },
            "scope": response.scope,
            "persistent": response.scope == PermissionGrantScope::Session,
            "strictAutoReview": response.strict_auto_review,
        })
        .to_string();
        ToolResult::completed(request, output, false)
    }

    pub fn execute_request_permissions_tool_with_policy(
        &mut self,
        request: &ToolRequest,
        approval_mode: ApprovalMode,
        handler: Option<&dyn RuntimePermissionRequestHandler>,
    ) -> ToolResult {
        match RuntimePermissionPolicy::decide_request_permissions_prompt(
            approval_mode,
            handler.is_some(),
        ) {
            RuntimePermissionPromptDecision::AutoAllow => {
                self.execute_request_permissions_tool(request)
            }
            RuntimePermissionPromptDecision::Prompt => self
                .execute_request_permissions_tool_with_handler(
                    request,
                    handler.expect("prompt decision requires a permission handler"),
                ),
            RuntimePermissionPromptDecision::Reject { reason } => {
                ToolResult::denied(request, reason)
            }
        }
    }

    pub fn execute_workflow_ipc_tool(
        &mut self,
        request: &ToolRequest,
        workflow_ipc: Option<&dyn RuntimeWorkflowIpc>,
    ) -> ToolResult {
        let Some(workflow_ipc) = workflow_ipc else {
            return ToolResult::failed(
                request,
                "workflow IPC tools are only available inside workflow child agents",
                None,
            );
        };
        let raw = request.raw_arguments.as_deref().unwrap_or("{}");
        let args: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::invalid_input(
                    request,
                    format!("arguments are not valid JSON: {error}"),
                );
            }
        };
        let result = match request.name {
            ToolName::WorkflowSendMessage => {
                let channel = match required_string_arg(request, &args, "channel") {
                    Ok(channel) => channel,
                    Err(result) => return result,
                };
                let message = args.get("message").cloned().unwrap_or(Value::Null);
                let from = args.get("from").and_then(Value::as_str);
                workflow_ipc.send_message(channel, from, message)
            }
            ToolName::WorkflowReadMessages => {
                let channel = match required_string_arg(request, &args, "channel") {
                    Ok(channel) => channel,
                    Err(result) => return result,
                };
                workflow_ipc.read_messages(channel)
            }
            ToolName::WorkflowClearMessages => {
                let channel = match required_string_arg(request, &args, "channel") {
                    Ok(channel) => channel,
                    Err(result) => return result,
                };
                workflow_ipc.clear_messages(channel)
            }
            ToolName::WorkflowCreateTaskList => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                let items = match args.get("items").and_then(Value::as_array) {
                    Some(items) => items.clone(),
                    None => {
                        return ToolResult::invalid_input(
                            request,
                            "missing required array field: items",
                        );
                    }
                };
                workflow_ipc.create_task_list(name, items)
            }
            ToolName::WorkflowClaimTask => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                let by = args.get("by").and_then(Value::as_str);
                workflow_ipc.claim_task(name, by)
            }
            ToolName::WorkflowCompleteTask => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                let task_id = match required_string_arg(request, &args, "task_id") {
                    Ok(task_id) => task_id,
                    Err(result) => return result,
                };
                let result = args.get("result").cloned().unwrap_or(Value::Null);
                let by = args.get("by").and_then(Value::as_str);
                workflow_ipc.complete_task(name, task_id, result, by)
            }
            ToolName::WorkflowListTasks => {
                let name = match required_string_arg(request, &args, "name") {
                    Ok(name) => name,
                    Err(result) => return result,
                };
                workflow_ipc.list_tasks(name)
            }
            _ => unreachable!("workflow IPC tool dispatch guarded by caller"),
        };

        match result {
            Ok(value) => ToolResult::completed(request, value.to_string(), false),
            Err(error) => ToolResult::invalid_input(request, error),
        }
    }

    pub fn execute_subagent_status_tool(
        &mut self,
        request: &ToolRequest,
        lookup: &dyn RuntimeSubagentStatusLookup,
    ) -> ToolResult {
        let args = match parse_subagent_status_request_args(request) {
            Ok(args) => args,
            Err(error) => return ToolResult::invalid_input(request, error),
        };
        let agent_id = args.agent_id.or_else(|| request.target.clone());
        let Some(agent_id) = agent_id else {
            return ToolResult::invalid_input(request, "missing agent_id");
        };
        let limit = args.limit.unwrap_or(DEFAULT_SUBAGENT_STATUS_PAGE_CHARS);
        if limit == 0 || limit > MAX_SUBAGENT_STATUS_PAGE_CHARS {
            return ToolResult::invalid_input(
                request,
                format!("limit must be between 1 and {MAX_SUBAGENT_STATUS_PAGE_CHARS}"),
            );
        }
        let offset = args.offset.unwrap_or_default();
        let Some(record) = lookup.subagent_status_record(&agent_id) else {
            return ToolResult::failed(request, format!("subagent '{agent_id}' not found"), None);
        };
        let result_page = record
            .output
            .as_deref()
            .map(|raw| page_async_subagent_result(raw, offset, limit));
        let error_page = record
            .error
            .as_deref()
            .map(|raw| page_async_subagent_result(raw, offset, limit));
        let output = json!({
            "agent_id": agent_id,
            "status": record.status,
            "description": record.description,
            "agent_type": record.agent_type,
            "created_at_ms": record.created_at_ms,
            "started_at_ms": record.started_at_ms,
            "completed_at_ms": record.completed_at_ms,
            "output": result_page.as_ref().and_then(|page| page.output.clone()),
            "output_total_chars": result_page.as_ref().and_then(|page| page.total_chars),
            "output_offset": result_page.as_ref().and_then(|page| page.offset),
            "output_next_offset": result_page.as_ref().and_then(|page| page.next_offset),
            "error": error_page.as_ref().and_then(|page| page.output.clone()),
            "error_total_chars": error_page.as_ref().and_then(|page| page.total_chars),
            "error_offset": error_page.as_ref().and_then(|page| page.offset),
            "error_next_offset": error_page.as_ref().and_then(|page| page.next_offset),
            "task": result_page
                .and_then(|page| page.task)
                .or_else(|| error_page.and_then(|page| page.task)),
            "usage": record.usage.map(runtime_usage_totals_json),
            "current_activity": record.subagent_current_activity,
            "activity_history": record.subagent_activity_history,
            "turn": record.subagent_turn,
            "last_activity_at_ms": record.last_activity_at_ms,
            "continuation_id": record.continuation_id,
            "attempt_id": record.continuation_attempt_id,
            "checkpoint_id": record.continuation_checkpoint_id,
            "resumable": record.continuation_resumable,
            "indeterminate": record.continuation_indeterminate,
        })
        .to_string();
        ToolResult::completed(request, output, false)
    }

    pub fn execute_task_list_tool(
        &mut self,
        request: &ToolRequest,
        task_registry: &TaskRegistry,
    ) -> ToolResult {
        let tasks = task_registry
            .list()
            .into_iter()
            .map(task_summary_json)
            .collect::<Vec<_>>();
        ToolResult::completed(request, json!({ "tasks": tasks }).to_string(), false)
    }

    pub fn execute_task_stop_tool(
        &mut self,
        request: &ToolRequest,
        task_registry: &TaskRegistry,
    ) -> ToolResult {
        let args = match parse_tool_arguments(request) {
            Ok(args) => args,
            Err(error) => return ToolResult::invalid_input(request, error),
        };
        let Some(task_id) = args
            .get("task_id")
            .and_then(Value::as_str)
            .or_else(|| args.get("shell_id").and_then(Value::as_str))
            .filter(|task_id| !task_id.trim().is_empty())
        else {
            return ToolResult::invalid_input(request, "missing required field: task_id");
        };
        let Some(record) = task_registry.get(task_id) else {
            return ToolResult::failed(request, format!("task '{task_id}' not found"), None);
        };
        if is_terminal_task_status(record.status) {
            return ToolResult::failed(
                request,
                format!(
                    "task is already {} and cannot be stopped",
                    task_status_label(record.status)
                ),
                None,
            );
        }
        let stopped_immediately = if record.status == TaskStatus::ApprovalRequired {
            if let Err(error) = task_registry.stop(task_id, "Task stopped".to_string()) {
                return ToolResult::failed(request, error, None);
            }
            true
        } else if let Err(error) = task_registry.request_stop(task_id) {
            return ToolResult::failed(request, error, None);
        } else {
            task_registry
                .get(task_id)
                .is_some_and(|record| record.status == TaskStatus::Stopped)
        };
        let output = json!({
            "message": if stopped_immediately {
                "Task stopped"
            } else {
                "Task stop requested"
            },
            "task_id": record.id,
            "task_type": task_type_label(record.task_type),
            "command": record.command,
        })
        .to_string();
        ToolResult::completed(request, output, false)
    }

    pub fn execute_workflow_draft_tool(
        &mut self,
        request: &ToolRequest,
        draft_request: RuntimeWorkflowDraftRequest<'_>,
    ) -> io::Result<ToolResult> {
        if !draft_request.workflows_enabled {
            return Ok(ToolResult::failed(request, "workflows are disabled", None));
        }
        let session_dir = draft_request
            .cwd
            .join(".orca")
            .join("workflow-sessions")
            .join(draft_request.session_id);
        self.execute_workflow_draft_tool_at(
            request,
            draft_request.cwd,
            draft_request.session_id,
            draft_request.max_concurrent_agents,
            &session_dir,
        )
    }

    pub(crate) fn execute_workflow_draft_tool_with_registry(
        &mut self,
        request: &ToolRequest,
        workflows_enabled: bool,
        cwd: &Path,
        task_registry: &TaskRegistry,
        max_concurrent_agents: usize,
    ) -> io::Result<ToolResult> {
        if !workflows_enabled {
            return Ok(ToolResult::failed(request, "workflows are disabled", None));
        }
        let session_dir = task_registry.workflow_session_dir(cwd)?;
        self.execute_workflow_draft_tool_at(
            request,
            cwd,
            task_registry.session_id(),
            max_concurrent_agents,
            &session_dir,
        )
    }

    fn execute_workflow_draft_tool_at(
        &mut self,
        request: &ToolRequest,
        cwd: &Path,
        session_id: &str,
        max_concurrent_agents: usize,
        session_dir: &Path,
    ) -> io::Result<ToolResult> {
        let script = workflow_draft_script_arg(request)?;
        let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
        let draft =
            draft_store.create_from_script(session_id, cwd, &script, max_concurrent_agents)?;
        let output = serde_json::to_string(&draft)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(ToolResult::completed(request, output, false))
    }
}

fn parse_runtime_permission_request(
    request: &ToolRequest,
) -> Result<RuntimePermissionRequestArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "missing request_permissions arguments JSON".to_string())?;
    let mut args: RuntimePermissionRequestArgs = serde_json::from_str(raw)
        .map_err(|error| format!("invalid request_permissions arguments JSON: {error}"))?;
    args.permissions = args.permissions.normalize_file_system_entries();
    if args
        .reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err("missing required request_permissions argument: reason".to_string());
    }
    let file_system = args.permissions.file_system.as_ref();
    let has_file_system_request = file_system.is_some_and(|file_system| {
        file_system
            .read
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
            || file_system
                .write
                .as_ref()
                .is_some_and(|paths| !paths.is_empty())
    });
    let has_network_request = args
        .permissions
        .network
        .as_ref()
        .is_some_and(|network| network.enabled.is_some() || !network.domains.is_empty());
    if !has_file_system_request && !has_network_request {
        return Err("request_permissions requires at least one permission request".to_string());
    }
    Ok(args)
}

fn required_string_arg<'a>(
    request: &ToolRequest,
    args: &'a Value,
    field: &str,
) -> Result<&'a str, ToolResult> {
    args.get(field).and_then(Value::as_str).ok_or_else(|| {
        ToolResult::invalid_input(request, format!("missing required string field: {field}"))
    })
}

fn parse_tool_arguments(request: &ToolRequest) -> Result<Value, String> {
    serde_json::from_str(request.raw_arguments.as_deref().unwrap_or("{}"))
        .map_err(|error| format!("arguments are not valid JSON: {error}"))
}

fn task_summary_json(task: BackgroundTaskSummary) -> Value {
    json!({
        "id": task.id,
        "subject": task.description,
        "status": task_status_label(task.status),
        "owner": Value::Null,
        "blockedBy": [],
        "task_type": task_type_label(task.task_type),
        "isBackgrounded": task.is_backgrounded,
        "command": task.command,
        "tool": task.tool,
        "pendingToolCall": task.pending_tool_call,
        "continuation": task.continuation,
    })
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Paused => "paused",
        TaskStatus::Stopping => "stopping",
        TaskStatus::Stopped => "stopped",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::ApprovalRequired => "approval_required",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn task_type_label(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::MainSession => "main_session",
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}

fn is_terminal_task_status(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Stopped | TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

fn parse_subagent_status_request_args(
    request: &ToolRequest,
) -> Result<SubagentStatusRequestArgs, String> {
    serde_json::from_str(request.raw_arguments.as_deref().unwrap_or("{}"))
        .map_err(|error| format!("arguments are not valid for subagent_status: {error}"))
}

fn runtime_usage_totals_json(usage: RuntimeUsageTotals) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_tokens": usage.cache_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
        "estimated_cost_usd": usage.estimated_cost_usd,
    })
}

fn unpack_async_subagent_result(raw: &str) -> (Option<Value>, Option<Value>) {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return (Some(Value::String(raw.to_string())), None);
    };
    let Some(output) = value.get("output") else {
        return (Some(Value::String(raw.to_string())), None);
    };
    let task = value.get("task").cloned().filter(|task| !task.is_null());
    (Some(output.clone()), task)
}

fn page_async_subagent_result(
    raw: &str,
    requested_offset: usize,
    limit: usize,
) -> PagedAsyncSubagentResult {
    let (output, task) = unpack_async_subagent_result(raw);
    let Some(Value::String(text)) = output else {
        return PagedAsyncSubagentResult {
            output,
            task,
            total_chars: None,
            offset: None,
            next_offset: None,
        };
    };

    let total_chars = text.chars().count();
    let offset = requested_offset.min(total_chars);
    let page: String = text.chars().skip(offset).take(limit).collect();
    let next_offset = offset
        .checked_add(page.chars().count())
        .filter(|next_offset| *next_offset < total_chars);
    PagedAsyncSubagentResult {
        output: Some(Value::String(page)),
        task,
        total_chars: Some(total_chars),
        offset: Some(offset),
        next_offset,
    }
}

fn workflow_draft_script_arg(request: &ToolRequest) -> io::Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WorkflowDraftInput {
        script: String,
    }

    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str::<WorkflowDraftInput>(raw_arguments)
        .map(|input| input.script)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::task_types::PendingToolCallSummary;
    use orca_core::tool_types::{ToolRequest, ToolStatus};

    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    #[cfg(unix)]
    use std::process::{Command, Stdio};
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[test]
    fn task_summary_json_marks_backgrounded_main_sessions() {
        let task = BackgroundTaskSummary {
            id: "task-main".to_string(),
            parent_task_id: None,
            task_type: TaskType::MainSession,
            status: TaskStatus::ApprovalRequired,
            is_backgrounded: true,
            description: "long turn".to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: Some(2_000),
            command: None,
            agent_type: Some("main-session".to_string()),
            server: None,
            tool: Some("task_list".to_string()),
            pending_tool_call: Some(PendingToolCallSummary {
                id: "mock-tool-1".to_string(),
                name: "task_list".to_string(),
                action: ActionKind::Read,
                target: None,
                arguments: "{}".to_string(),
            }),
            name: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_activity_history: Vec::new(),
            subagent_child_thread_id: None,
            subagent_batch_id: None,
            subagent_batch_size: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            publication_revision: None,
        };

        let summary = task_summary_json(task);

        assert_eq!(summary["task_type"], "main_session");
        assert_eq!(summary["status"], "approval_required");
        assert_eq!(summary["isBackgrounded"], true);
        assert_eq!(summary["tool"], "task_list");
        assert_eq!(summary["pendingToolCall"]["id"], "mock-tool-1");
        assert_eq!(summary["pendingToolCall"]["name"], "task_list");
        assert_eq!(summary["pendingToolCall"]["action"], "read");
        assert_eq!(summary["pendingToolCall"]["arguments"], "{}");
    }

    #[test]
    fn workflow_draft_registry_entrypoint_uses_releasable_process_local_storage() {
        let cwd = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new("ephemeral-thread".to_string());
        let request = ToolRequest {
            id: "draft".to_string(),
            name: ToolName::WorkflowDraft,
            action: ActionKind::Write,
            target: Some("preview workflow".to_string()),
            raw_arguments: Some(
                json!({
                    "script": "export const meta = { name: 'ephemeral', description: 'Ephemeral draft', phases: ['main'] };\nexport default 'done';"
                })
                .to_string(),
            ),
        };
        let mut context = RuntimeToolActorContext::new("test-run");

        let result = context
            .execute_workflow_draft_tool_with_registry(&request, true, cwd.path(), &registry, 3)
            .unwrap();

        assert_eq!(result.status, ToolStatus::Completed);
        let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
        let script_path = Path::new(output["scriptPath"].as_str().unwrap());
        assert!(script_path.is_file());
        assert!(!script_path.starts_with(cwd.path()));
        assert!(!cwd.path().join(".orca").exists());

        let session_dir = script_path
            .parent()
            .and_then(Path::parent)
            .expect("workflow session directory")
            .to_path_buf();
        registry.release_process_local_artifacts();
        assert!(!session_dir.exists());
    }

    #[test]
    fn task_stop_stops_approval_required_background_main_session() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("waiting for approval".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_tool(
                &task.id,
                "approval_required".to_string(),
                Some("task_list".to_string()),
            )
            .unwrap();
        let request = ToolRequest {
            id: "call-stop".to_string(),
            name: ToolName::TaskStop,
            action: orca_core::approval_types::ActionKind::Write,
            target: None,
            raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task.id)),
        };
        let mut context = RuntimeToolActorContext::new("test-run");

        let result = context.execute_task_stop_tool(&request, &registry);

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
        assert_eq!(output["message"], "Task stopped");
        let stopped = registry.get(&task.id).unwrap();
        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert_eq!(stopped.result.as_deref(), Some("Task stopped"));
        assert_eq!(stopped.error, None);
    }

    #[cfg(unix)]
    #[test]
    fn task_stop_terminates_owned_async_subagent_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let signal_file = temp.path().join("signals");
        let ready_file = temp.path().join("ready");
        let descendant_pid_file = temp.path().join("descendant-pid");
        let descendant_ready_file = temp.path().join("descendant-ready");
        let mut command = Command::new("sh");
        command
            .env("ORCA_TEST_SIGNAL_FILE", &signal_file)
            .env("ORCA_TEST_READY_FILE", &ready_file)
            .env("ORCA_TEST_DESCENDANT_PID_FILE", &descendant_pid_file)
            .env("ORCA_TEST_DESCENDANT_READY_FILE", &descendant_ready_file)
            .arg("-c")
            .arg(
                r#"
trap 'printf "worker\n" >> "$ORCA_TEST_SIGNAL_FILE"; exit 0' TERM
sh -c 'trap '\''printf "descendant\n" >> "$ORCA_TEST_SIGNAL_FILE"; exit 0'\'' TERM; printf "ready\n" > "$ORCA_TEST_DESCENDANT_READY_FILE"; while :; do :; done' &
printf '%s\n' "$!" > "$ORCA_TEST_DESCENDANT_PID_FILE"
while [ ! -e "$ORCA_TEST_DESCENDANT_READY_FILE" ]; do sleep 0.01; done
printf 'ready\n' > "$ORCA_TEST_READY_FILE"
while :; do :; done
"#,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command.spawn().expect("spawn async subagent fixture");
        let child_pid = i32::try_from(child.id()).expect("child PID fits pid_t");
        assert_eq!(
            unsafe { libc::getpgid(child_pid) },
            child_pid,
            "async subagent worker must lead its own process group"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready_file.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready_file.exists(), "async subagent fixture did not start");
        let descendant_pid = fs::read_to_string(&descendant_pid_file)
            .expect("descendant PID")
            .trim()
            .parse::<i32>()
            .expect("valid descendant PID");
        assert_eq!(
            unsafe { libc::getpgid(descendant_pid) },
            child_pid,
            "async subagent descendant must stay in the worker process group"
        );

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_subagent("long-running async work".to_string(), None);
        registry
            .adopt_subagent_worker(&task.id, child)
            .expect("adopt async subagent worker");
        registry.mark_running(&task.id).unwrap();
        let request = ToolRequest {
            id: "call-stop-async".to_string(),
            name: ToolName::TaskStop,
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task.id)),
        };
        let mut context = RuntimeToolActorContext::new("test-run");

        let result = context.execute_task_stop_tool(&request, &registry);

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
        assert_eq!(output["message"], "Task stopped");
        let stopped = registry.get(&task.id).unwrap();
        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert_eq!(stopped.result.as_deref(), Some("Task stopped"));
        assert_eq!(stopped.worker_pid, None);
        assert!(stopped.control.cancel.is_cancelled());
        let signals = fs::read_to_string(&signal_file).unwrap_or_default();
        assert!(signals.contains("worker"), "worker did not receive TERM");
        assert!(
            signals.contains("descendant"),
            "worker descendant did not receive process-group TERM"
        );
    }

    #[test]
    fn task_stop_preserves_in_process_cancellation_semantics() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            1,
        );
        registry.mark_running(&task.id).unwrap();
        let request = ToolRequest {
            id: "call-stop-workflow".to_string(),
            name: ToolName::TaskStop,
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task.id)),
        };
        let mut context = RuntimeToolActorContext::new("test-run");

        let result = context.execute_task_stop_tool(&request, &registry);

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
        assert_eq!(output["message"], "Task stop requested");
        let stopping = registry.get(&task.id).unwrap();
        assert_eq!(stopping.status, TaskStatus::Stopping);
        assert!(stopping.control.cancel.is_cancelled());
    }

    #[test]
    fn request_permissions_without_handler_is_rejected_outside_full_auto() {
        let request = ToolRequest {
            id: "permission-1".to_string(),
            name: ToolName::RequestPermissions,
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "reason": "need workspace write",
                    "permissions": {
                        "fileSystem": { "write": ["/tmp/orca-write"], "read": null },
                        "network": null
                    }
                })
                .to_string(),
            ),
        };
        let mut context = RuntimeToolActorContext::new("test-run");

        let result = context.execute_request_permissions_tool_with_policy(
            &request,
            ApprovalMode::Suggest,
            None,
        );

        assert_eq!(result.status, ToolStatus::Denied);
        assert_eq!(
            result.error.as_deref(),
            Some(
                "request_permissions requires a runtime permission handler unless approval mode is full-auto"
            )
        );
        assert!(
            context
                .permission_overlay()
                .additional_working_directories()
                .is_empty()
        );
    }
}
