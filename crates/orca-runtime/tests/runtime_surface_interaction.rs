use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use orca_core::approval_types::{
    ActionKind, ApprovalDecision, ApprovalMode, ApprovalRequest, ApprovalResolution,
};
use orca_core::cancel::CancelToken;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::conversation::RawToolCall;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::model::ModelSelection;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::subagent_config::SubagentConfig;
use orca_core::thread_item_projection::ModelResponseIdentity;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_mcp::{McpElicitationMode, McpElicitationRequest, McpElicitationResponse};
use orca_runtime::lifecycle::RuntimeUserInputRequest;
use orca_runtime::model_response::RuntimeModelResponse;
use orca_runtime::protocol::{
    PermissionGrantScope as RuntimePermissionGrantScope, PermissionResponseDecision,
    RequestFileSystemPermissions, RequestPermissionProfile,
};
use orca_runtime::runtime_host::{
    GenerationContext, HostedTurnRequest, RuntimeHost, ThreadOperationExecutor,
    ThreadOperationOutcome,
};
use orca_runtime::runtime_permission::{RuntimePermissionRequest, RuntimePermissionResponse};
use orca_runtime::surface::*;
use orca_runtime::thread::RuntimeThread;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());
const INTERACTION_RESTART_CHILD: &str = "ORCA_INTERACTION_RESTART_CHILD";
const RESOLVED_INTERACTION_RESTART_CHILD: &str = "ORCA_RESOLVED_INTERACTION_RESTART_CHILD";
const EFFECT_APPROVAL_RESTART_CHILD: &str = "ORCA_EFFECT_APPROVAL_RESTART_CHILD";
const RESOLVED_EFFECT_APPROVAL_RESTART_CHILD: &str = "ORCA_RESOLVED_EFFECT_APPROVAL_RESTART_CHILD";
const EFFECT_PERMISSION_RESTART_CHILD: &str = "ORCA_EFFECT_PERMISSION_RESTART_CHILD";
const RESOLVED_EFFECT_PERMISSION_RESTART_CHILD: &str =
    "ORCA_RESOLVED_EFFECT_PERMISSION_RESTART_CHILD";
const TOOL_COMPLETION_RESTART_CHILD: &str = "ORCA_TOOL_COMPLETION_RESTART_CHILD";
const ASSISTANT_STREAM_RESTART_CHILD: &str = "ORCA_ASSISTANT_STREAM_RESTART_CHILD";

struct UserInputExecutor {
    answer_tx: mpsc::SyncSender<Option<String>>,
}

struct PanicExecutor;

impl ThreadOperationExecutor for PanicExecutor {
    fn run_turn(
        &self,
        _thread: &mut RuntimeThread,
        _request: &HostedTurnRequest,
        _generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        panic!("cold interaction recovery must not rerun the generation")
    }
}

struct McpExecutor {
    response_tx: mpsc::SyncSender<McpElicitationResponse>,
}

struct CrossKindReuseExecutor {
    result_tx: mpsc::SyncSender<(Option<String>, McpElicitationResponse)>,
}

struct BlockingResolvedUserInputExecutor {
    answer_tx: mpsc::SyncSender<Option<String>>,
}

struct ToolApprovalExecutor {
    resolution_tx: mpsc::SyncSender<ApprovalResolution>,
}

struct ToolCompletionExecutor;

struct FailedToolCompletionExecutor;

struct WrongToolCompletionExecutor;

struct NonShellToolCompletionExecutor;

struct ReadonlyBatchCompletionExecutor;

struct AssistantStreamingExecutor;

struct RecoveredAssistantStreamingExecutor;

struct AbandonedAssistantStreamExecutor;

struct BlockingAssistantStreamExecutor;

struct PermissionExecutor {
    response_tx: mpsc::SyncSender<RuntimePermissionResponse>,
    tool: ToolRequest,
}

struct BlockingResolvedToolApprovalExecutor {
    resolution_tx: mpsc::SyncSender<ApprovalResolution>,
}

struct BlockingResolvedPermissionExecutor {
    response_tx: mpsc::SyncSender<RuntimePermissionResponse>,
}

fn effect_tool_request() -> ToolRequest {
    ToolRequest {
        id: "effect-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf owned".to_string()),
        raw_arguments: Some(r#"{"command":"printf owned"}"#.to_string()),
    }
}

fn permission_tool_request() -> ToolRequest {
    ToolRequest {
        id: "permission-1".to_string(),
        name: ToolName::RequestPermissions,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some("{}".to_string()),
    }
}

fn provider_response_for_tool(
    request: &ToolRequest,
    turn_id: orca_core::thread_identity::TurnId,
) -> RuntimeModelResponse {
    RuntimeModelResponse::new(
        ProviderResponse {
            steps: vec![ProviderStep::ToolCall(request.clone())],
            assistant_content: Some("I need to use a tool.".to_string()),
            assistant_reasoning: Some("The requested action is required.".to_string()),
            tool_calls: vec![RawToolCall {
                id: request.id.clone(),
                function_name: request.name.as_str().to_string(),
                arguments: request.raw_arguments.clone().unwrap_or_default(),
            }],
            usage: None,
        },
        turn_id,
    )
}

fn provider_response_for_tools(
    requests: &[ToolRequest],
    turn_id: orca_core::thread_identity::TurnId,
) -> RuntimeModelResponse {
    RuntimeModelResponse::new(
        ProviderResponse {
            steps: requests
                .iter()
                .cloned()
                .map(ProviderStep::ToolCall)
                .collect(),
            assistant_content: Some("I need to read two inputs.".to_string()),
            assistant_reasoning: None,
            tool_calls: requests
                .iter()
                .map(|request| RawToolCall {
                    id: request.id.clone(),
                    function_name: request.name.as_str().to_string(),
                    arguments: request.raw_arguments.clone().unwrap_or_default(),
                })
                .collect(),
            usage: None,
        },
        turn_id,
    )
}

impl ThreadOperationExecutor for ToolApprovalExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let turn_request = request.thread_turn_request(generation);
        let tool = effect_tool_request();
        turn_request
            .provider_response_ingress()
            .expect("typed generation installs provider response ingress")
            .commit_response(&provider_response_for_tool(
                &tool,
                request.turn_id().clone(),
            ))?;
        let resolution = turn_request
            .approval_handler()
            .expect("typed generation installs runtime-owned approval broker")
            .resolve_interactive(
                &ApprovalRequest {
                    id: "approval-effect-1".to_string(),
                    action: ActionKind::Shell,
                    description: "run effect-bearing command".to_string(),
                    tool: Some(tool.name.as_str().to_string()),
                    target: tool.target.clone(),
                    preview: None,
                },
                &tool,
            )?;
        self.resolution_tx.send(resolution).unwrap();
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

impl ThreadOperationExecutor for ToolCompletionExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let turn_request = request.thread_turn_request(generation);
        let tool = effect_tool_request();
        let ingress = turn_request
            .provider_response_ingress()
            .expect("typed generation installs semantic ingress");
        ingress.commit_response(&provider_response_for_tool(
            &tool,
            request.turn_id().clone(),
        ))?;
        ingress.commit_tool_result(&ToolResult::completed(
            &tool,
            "owned output".to_string(),
            false,
        ))?;
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

impl ThreadOperationExecutor for FailedToolCompletionExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let turn_request = request.thread_turn_request(generation);
        let tool = effect_tool_request();
        let ingress = turn_request
            .provider_response_ingress()
            .expect("typed generation installs semantic ingress");
        ingress.commit_response(&provider_response_for_tool(
            &tool,
            request.turn_id().clone(),
        ))?;
        ingress.commit_tool_result(&ToolResult::failed_after_start(
            &tool,
            "effect failed",
            Some(17),
        ))?;
        thread.lifecycle_mut().finish_task(RunStatus::Failed);
        Ok(RunStatus::Failed.into())
    }
}

impl ThreadOperationExecutor for WrongToolCompletionExecutor {
    fn run_turn(
        &self,
        _thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let turn_request = request.thread_turn_request(generation);
        let tool = effect_tool_request();
        let ingress = turn_request
            .provider_response_ingress()
            .expect("typed generation installs semantic ingress");
        ingress.commit_response(&provider_response_for_tool(
            &tool,
            request.turn_id().clone(),
        ))?;
        let mut wrong_tool = tool.clone();
        wrong_tool.id = "effect-other".to_string();
        ingress.commit_tool_result(&ToolResult::completed(
            &wrong_tool,
            "must not commit".to_string(),
            false,
        ))?;
        unreachable!("wrong tool completion must be rejected")
    }
}

impl ThreadOperationExecutor for NonShellToolCompletionExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let turn_request = request.thread_turn_request(generation);
        let tool = ToolRequest {
            id: "read-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("notes.txt".to_string()),
            raw_arguments: Some(r#"{"path":"notes.txt"}"#.to_string()),
        };
        let ingress = turn_request
            .provider_response_ingress()
            .expect("typed generation installs semantic ingress");
        ingress.commit_response(&provider_response_for_tool(
            &tool,
            request.turn_id().clone(),
        ))?;
        ingress.commit_tool_result(&ToolResult::completed(&tool, "notes".to_string(), false))?;
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

impl ThreadOperationExecutor for ReadonlyBatchCompletionExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let requests = [
            ToolRequest {
                id: "read-batch-1".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("one.txt".to_string()),
                raw_arguments: Some(r#"{"path":"one.txt"}"#.to_string()),
            },
            ToolRequest {
                id: "read-batch-2".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("two.txt".to_string()),
                raw_arguments: Some(r#"{"path":"two.txt"}"#.to_string()),
            },
        ];
        let turn_request = request.thread_turn_request(generation);
        let ingress = turn_request
            .provider_response_ingress()
            .expect("typed generation installs semantic ingress");
        ingress.commit_response(&provider_response_for_tools(
            &requests,
            request.turn_id().clone(),
        ))?;
        ingress.commit_tool_results(&[
            ToolResult::completed(&requests[0], "one".to_string(), false),
            ToolResult::completed(&requests[1], "two".to_string(), false),
        ])?;
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

impl ThreadOperationExecutor for AssistantStreamingExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let identity = ModelResponseIdentity::new(request.turn_id().clone());
        let turn_request = request.thread_turn_request(generation);
        let ingress = turn_request
            .provider_response_ingress()
            .expect("typed generation installs semantic ingress");
        for step in [
            ProviderStep::MessageDelta("hel".to_string()),
            ProviderStep::ReasoningDelta("thi".to_string()),
            ProviderStep::MessageDelta("lo".to_string()),
            ProviderStep::ReasoningDelta("nk".to_string()),
        ] {
            ingress.commit_provider_step(&identity, &step)?;
        }
        ingress.commit_response(&RuntimeModelResponse::from_parts(
            ProviderResponse {
                steps: Vec::new(),
                assistant_content: Some("hello".to_string()),
                assistant_reasoning: Some("think".to_string()),
                tool_calls: Vec::new(),
                usage: None,
            },
            identity,
        ))?;
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

impl ThreadOperationExecutor for RecoveredAssistantStreamingExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let identity = ModelResponseIdentity::new(request.turn_id().clone());
        let turn_request = request.thread_turn_request(generation);
        let ingress = turn_request
            .provider_response_ingress()
            .expect("typed generation installs semantic ingress");
        ingress.commit_provider_step(
            &identity,
            &ProviderStep::ReasoningDelta("first attempt thinking".to_string()),
        )?;
        ingress.commit_provider_step(
            &identity,
            &ProviderStep::MessageDelta("recovered".to_string()),
        )?;
        ingress.commit_response(&RuntimeModelResponse::from_parts(
            ProviderResponse {
                steps: Vec::new(),
                assistant_content: Some("recovered".to_string()),
                assistant_reasoning: Some("retry thinking".to_string()),
                tool_calls: Vec::new(),
                usage: None,
            },
            identity,
        ))?;
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

impl ThreadOperationExecutor for AbandonedAssistantStreamExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let identity = ModelResponseIdentity::new(request.turn_id().clone());
        request
            .thread_turn_request(generation)
            .provider_response_ingress()
            .expect("typed generation installs semantic ingress")
            .commit_provider_step(
                &identity,
                &ProviderStep::MessageDelta("partial".to_string()),
            )?;
        thread.lifecycle_mut().finish_task(RunStatus::Failed);
        Ok(RunStatus::Failed.into())
    }
}

impl ThreadOperationExecutor for BlockingAssistantStreamExecutor {
    fn run_turn(
        &self,
        _thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let identity = ModelResponseIdentity::new(request.turn_id().clone());
        let turn_request = request.thread_turn_request(generation);
        let ingress = turn_request
            .provider_response_ingress()
            .expect("typed generation installs semantic ingress");
        ingress.commit_provider_step(
            &identity,
            &ProviderStep::MessageDelta("restart-partial".to_string()),
        )?;
        ingress
            .commit_provider_step(&identity, &ProviderStep::ReasoningDelta("sk-".to_string()))?;
        ingress.commit_provider_step(
            &identity,
            &ProviderStep::ReasoningDelta("opaquevalue1234567890 ".to_string()),
        )?;
        std::thread::park();
        unreachable!("restart fixture exits while assistant stream is open")
    }
}

impl ThreadOperationExecutor for PermissionExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let turn_request = request.thread_turn_request(generation);
        let tool = self.tool.clone();
        turn_request
            .provider_response_ingress()
            .expect("typed generation installs provider response ingress")
            .commit_response(&provider_response_for_tool(
                &tool,
                request.turn_id().clone(),
            ))?;
        let response = turn_request
            .permission_handler()
            .expect("typed generation installs runtime-owned permission broker")
            .request_permissions(&RuntimePermissionRequest {
                id: tool.id.clone(),
                reason: Some("write generated output".to_string()),
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![PathBuf::from("/workspace/output")]),
                        entries: None,
                    }),
                    network: None,
                    shell: None,
                },
            })?;
        self.response_tx.send(response).unwrap();
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

impl ThreadOperationExecutor for BlockingResolvedToolApprovalExecutor {
    fn run_turn(
        &self,
        _thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let turn_request = request.thread_turn_request(generation);
        let tool = effect_tool_request();
        turn_request
            .provider_response_ingress()
            .unwrap()
            .commit_response(&provider_response_for_tool(
                &tool,
                request.turn_id().clone(),
            ))?;
        let resolution = turn_request
            .approval_handler()
            .unwrap()
            .resolve_interactive(
                &ApprovalRequest {
                    id: "approval-effect-1".to_string(),
                    action: ActionKind::Shell,
                    description: "run effect-bearing command".to_string(),
                    tool: Some(tool.name.as_str().to_string()),
                    target: tool.target.clone(),
                    preview: None,
                },
                &tool,
            )?;
        self.resolution_tx.send(resolution).unwrap();
        std::thread::park();
        unreachable!("restart fixture exits while effect generation is blocked")
    }
}

impl ThreadOperationExecutor for BlockingResolvedPermissionExecutor {
    fn run_turn(
        &self,
        _thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let turn_request = request.thread_turn_request(generation);
        let tool = permission_tool_request();
        turn_request
            .provider_response_ingress()
            .unwrap()
            .commit_response(&provider_response_for_tool(
                &tool,
                request.turn_id().clone(),
            ))?;
        let response = turn_request
            .permission_handler()
            .unwrap()
            .request_permissions(&RuntimePermissionRequest {
                id: tool.id,
                reason: Some("write generated output".to_string()),
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![PathBuf::from("/workspace/output")]),
                        entries: None,
                    }),
                    network: None,
                    shell: None,
                },
            })?;
        self.response_tx.send(response).unwrap();
        std::thread::park();
        unreachable!("restart fixture exits while permission generation is blocked")
    }
}

impl ThreadOperationExecutor for BlockingResolvedUserInputExecutor {
    fn run_turn(
        &self,
        _thread: &mut RuntimeThread,
        _request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let answer = generation
            .user_input_handler()
            .unwrap()
            .request_user_input(&RuntimeUserInputRequest {
                id: "input-1".to_string(),
                question: "Persist live-only winner?".to_string(),
                choices: Vec::new(),
            })?;
        self.answer_tx.send(answer).unwrap();
        std::thread::park();
        unreachable!("restart fixture exits the process while generation is blocked")
    }
}

impl ThreadOperationExecutor for McpExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        _request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let response = generation
            .mcp_elicitation_handler()
            .expect("runtime installs an MCP elicitation broker")
            .handle_elicitation(McpElicitationRequest {
                server_name: "docs".to_string(),
                id: "mcp-1".to_string(),
                mode: McpElicitationMode::Url,
                message: "Open sign-in?".to_string(),
                url: Some("https://example.com/sign-in".to_string()),
                requested_schema: None,
            })
            .map_err(io::Error::other)?;
        self.response_tx.send(response).unwrap();
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

impl ThreadOperationExecutor for CrossKindReuseExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        _request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let user_input = generation
            .user_input_handler()
            .expect("runtime installs a user-input broker")
            .request_user_input(&RuntimeUserInputRequest {
                id: "shared".to_string(),
                question: "First interaction?".to_string(),
                choices: Vec::new(),
            })?;
        let mcp = generation
            .mcp_elicitation_handler()
            .expect("runtime installs an MCP elicitation broker")
            .handle_elicitation(McpElicitationRequest {
                server_name: "docs".to_string(),
                id: "shared".to_string(),
                mode: McpElicitationMode::Url,
                message: "Second interaction?".to_string(),
                url: Some("https://example.com/reuse".to_string()),
                requested_schema: None,
            })
            .map_err(io::Error::other)?;
        self.result_tx.send((user_input, mcp)).unwrap();
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

impl ThreadOperationExecutor for UserInputExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        _request: &HostedTurnRequest,
        generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let answer = generation
            .user_input_handler()
            .expect("runtime installs a user-input broker for typed foreground generations")
            .request_user_input(&RuntimeUserInputRequest {
                id: "input-1".to_string(),
                question: "Ship this change?".to_string(),
                choices: vec!["yes".to_string(), "no".to_string()],
            })?;
        let status = if answer.is_some() {
            RunStatus::Success
        } else {
            RunStatus::Cancelled
        };
        self.answer_tx.send(answer).unwrap();
        thread.lifecycle_mut().finish_task(status);
        Ok(status.into())
    }
}

#[test]
fn provider_response_commits_tool_request_before_approval_can_publish() {
    let cwd = tempfile::tempdir().unwrap();
    let (resolution_tx, resolution_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(ToolApprovalExecutor { resolution_tx }))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned tool approval",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("claim subscription once");
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "approve exact effect"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );

    let interaction = collect_effect_interaction(
        &mut subscription,
        SurfaceInteractionKind::ToolApproval,
        "effect-1",
    );
    assert!(resolution_rx.try_recv().is_err());
    assert!(matches!(
        interaction.request,
        SurfaceInteractionRequest::ToolApproval { ref tool, .. }
            if tool.tool_call_id == SurfaceToolCallId::try_new("effect-1").unwrap()
                && tool.target.as_ref().map(DisplayText::as_str) == Some("printf owned")
    ));

    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                interaction.interaction_id,
                SurfaceClientInteractionAnswer::ToolApproval {
                    decision: SurfaceAllowDeny::Allow,
                },
            )
            .unwrap(),
    );
    assert_eq!(
        resolution_rx.recv_timeout(TEST_TIMEOUT).unwrap().decision,
        ApprovalDecision::Allow
    );
    let terminal = attachment
        .client
        .wait_operation_terminal(request_id(), operation_id)
        .unwrap();
    assert!(matches!(
        terminal,
        WaitOperationTerminalResult::Terminal { .. }
    ));
    host.shutdown().unwrap();
}

#[test]
fn tool_completion_and_result_item_commit_before_operation_terminal() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(ToolCompletionExecutor))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned tool completion",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "complete exact tool"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));

    let snapshot = fresh_snapshot(&surface);
    let tool = snapshot
        .tools
        .iter()
        .find(|tool| tool.request.tool_call_id == SurfaceToolCallId::try_new("effect-1").unwrap())
        .expect("completed tool remains projected");
    let result = tool.result.as_ref().expect("tool terminal is durable");
    assert_eq!(
        result.output.as_ref().map(DisplayText::as_str),
        Some("owned output")
    );
    assert!(snapshot.items.iter().any(|item| matches!(
        item,
        SurfaceItem::ToolResultMessage {
            tool_call_id,
            content,
            terminal,
            ..
        } if tool_call_id == &result.tool_call_id
            && content.as_str() == "owned output"
            && terminal == &result.terminal
    )));
    host.shutdown().unwrap();
}

#[test]
fn assistant_streams_are_durable_and_complete_into_exact_items() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(AssistantStreamingExecutor))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned assistant streams",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(
                    &attachment.baseline.snapshot,
                    "stream exact assistant facts",
                ),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));

    let snapshot = fresh_snapshot(&surface);
    assert_eq!(snapshot.assistant_streams.len(), 2);
    assert!(snapshot.assistant_streams.iter().any(|stream| {
        stream.channel == AssistantChannel::Message
            && stream.text.as_str() == "hello"
            && stream.next_offset == ByteOffset::new(5)
            && stream.state == SurfaceAssistantStreamState::Completed
    }));
    assert!(snapshot.assistant_streams.iter().any(|stream| {
        stream.channel == AssistantChannel::Reasoning
            && stream.text.as_str() == "think"
            && stream.next_offset == ByteOffset::new(5)
            && stream.state == SurfaceAssistantStreamState::Completed
    }));
    assert!(snapshot.items.iter().any(|item| matches!(
        item,
        SurfaceItem::AssistantMessage { text, .. } if text.as_str() == "hello"
    )));
    assert!(snapshot.items.iter().any(|item| matches!(
        item,
        SurfaceItem::AssistantReasoning { summary, content, .. }
            if summary.as_str().is_empty() && content.as_str() == "think"
    )));
    host.shutdown().unwrap();
}

#[test]
fn provider_recovery_discards_superseded_stream_before_completing_response() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(RecoveredAssistantStreamingExecutor))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "replace superseded assistant stream",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(
                    &attachment.baseline.snapshot,
                    "recover empty provider response",
                ),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));

    let snapshot = fresh_snapshot(&surface);
    assert!(snapshot.assistant_streams.iter().any(|stream| {
        stream.channel == AssistantChannel::Reasoning
            && stream.text.as_str() == "first attempt thinking"
            && stream.state == SurfaceAssistantStreamState::Discarded
    }));
    assert!(snapshot.assistant_streams.iter().any(|stream| {
        stream.channel == AssistantChannel::Message
            && stream.text.as_str() == "recovered"
            && stream.state == SurfaceAssistantStreamState::Completed
    }));
    assert!(snapshot.items.iter().any(|item| matches!(
        item,
        SurfaceItem::AssistantReasoning { summary, content, .. }
            if summary.as_str().is_empty() && content.as_str() == "retry thinking"
    )));
    host.shutdown().unwrap();
}

#[test]
fn transient_provider_attempt_is_discarded_and_retried_with_new_response_identity() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start().expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "retry transient provider attempt",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let prompt = format!("mock_stream_flaky_once {}", uuid::Uuid::new_v4());
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, &prompt),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));

    let snapshot = fresh_snapshot(&surface);
    assert!(snapshot.assistant_streams.iter().any(|stream| {
        stream.channel == AssistantChannel::Reasoning
            && stream.text.as_str() == "Mock transient attempt emitted partial reasoning."
            && stream.state == SurfaceAssistantStreamState::Discarded
    }));
    assert!(snapshot.assistant_streams.iter().any(|stream| {
        stream.channel == AssistantChannel::Message
            && stream
                .text
                .as_str()
                .contains("Mock runtime completed after stream recovery")
            && stream.state == SurfaceAssistantStreamState::Completed
    }));
    assert!(!snapshot.items.iter().any(|item| matches!(
        item,
        SurfaceItem::AssistantReasoning { summary, content, .. }
            if summary.as_str().contains("transient attempt")
                || content.as_str().contains("transient attempt")
    )));
    host.shutdown().unwrap();
}

#[test]
fn failed_generation_discards_open_assistant_stream_before_terminal() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(AbandonedAssistantStreamExecutor))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "discard abandoned assistant stream",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(
                    &attachment.baseline.snapshot,
                    "fail after a partial response",
                ),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));

    let snapshot = fresh_snapshot(&surface);
    assert_eq!(snapshot.assistant_streams.len(), 1);
    assert_eq!(
        snapshot.assistant_streams[0].state,
        SurfaceAssistantStreamState::Discarded
    );
    host.shutdown().unwrap();
}

#[test]
fn cold_recovery_discards_durable_partial_assistant_stream() {
    if std::env::var_os(ASSISTANT_STREAM_RESTART_CHILD).is_some() {
        run_assistant_stream_restart_child();
    }
    with_orca_home(|home| {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("cold_recovery_discards_durable_partial_assistant_stream")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(ASSISTANT_STREAM_RESTART_CHILD, "1")
            .env("ORCA_HOME", home)
            .status()
            .expect("start partial assistant restart fixture");
        assert!(status.success(), "partial assistant restart child failed");
        let thread_id: String = serde_json::from_slice(
            &fs::read(home.join("runtime-surface-assistant-stream-restart.json")).unwrap(),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor)).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover partial assistant stream",
            )
            .unwrap();
        let snapshot = fresh_snapshot(&thread.surface());
        assert_eq!(snapshot.assistant_streams.len(), 2);
        assert!(snapshot.assistant_streams.iter().any(|stream| {
            stream.channel == AssistantChannel::Message
                && stream.text.as_str() == "restart-partial"
                && stream.state == SurfaceAssistantStreamState::Discarded
        }));
        assert!(snapshot.assistant_streams.iter().any(|stream| {
            stream.channel == AssistantChannel::Reasoning
                && stream.text.as_str() == "<redacted> "
                && stream.state == SurfaceAssistantStreamState::Discarded
        }));
        assert!(
            snapshot
                .assistant_streams
                .iter()
                .all(|stream| !stream.text.as_str().contains("opaquevalue1234567890")),
            "restart must not recover a raw partial secret"
        );
        assert!(
            snapshot
                .operation_history
                .iter()
                .any(|operation| operation.terminal.is_some())
        );
        host.shutdown().unwrap();
    });
}

#[test]
fn failed_tool_completion_preserves_exact_terminal_facts() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(FailedToolCompletionExecutor)).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned failed tool completion",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "fail exact tool"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));

    let snapshot = fresh_snapshot(&surface);
    let result = snapshot
        .tools
        .iter()
        .find(|tool| tool.request.tool_call_id == SurfaceToolCallId::try_new("effect-1").unwrap())
        .and_then(|tool| tool.result.as_ref())
        .expect("failed tool terminal is durable");
    assert!(matches!(
        result.terminal.kind,
        SurfaceToolResultKind::Failed
    ));
    assert_eq!(
        result.error.as_ref().map(DisplayText::as_str),
        Some("effect failed")
    );
    assert_eq!(result.exit_code, Some(17));
    assert!(snapshot.items.iter().any(|item| matches!(
        item,
        SurfaceItem::ToolResultMessage {
            tool_call_id,
            content,
            terminal,
            ..
        } if tool_call_id == &result.tool_call_id
            && content.as_str() == "effect failed"
            && terminal == &result.terminal
    )));
    host.shutdown().unwrap();
}

#[test]
fn tool_completion_rejects_uncommitted_tool_identity() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(WrongToolCompletionExecutor)).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "reject wrong tool completion",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "reject wrong tool"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));

    let snapshot = fresh_snapshot(&surface);
    let tool = snapshot
        .tools
        .iter()
        .find(|tool| tool.request.tool_call_id == SurfaceToolCallId::try_new("effect-1").unwrap())
        .expect("provider tool request remains durable");
    assert!(tool.result.is_none());
    assert!(!snapshot.tools.iter().any(|tool| {
        tool.request.tool_call_id == SurfaceToolCallId::try_new("effect-other").unwrap()
    }));
    host.shutdown().unwrap();
}

#[test]
fn non_shell_completion_drops_generic_core_exit_code() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(NonShellToolCompletionExecutor)).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned read completion",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "read exact tool"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));

    let snapshot = fresh_snapshot(&surface);
    let result = snapshot
        .tools
        .iter()
        .find(|tool| tool.request.tool_call_id == SurfaceToolCallId::try_new("read-1").unwrap())
        .and_then(|tool| tool.result.as_ref())
        .expect("read terminal is durable");
    assert_eq!(result.exit_code, None);
    assert_eq!(
        result.output.as_ref().map(DisplayText::as_str),
        Some("notes")
    );
    host.shutdown().unwrap();
}

#[test]
fn completed_tool_lifecycle_recovers_without_generation_reexecution() {
    if std::env::var_os(TOOL_COMPLETION_RESTART_CHILD).is_some() {
        run_tool_completion_restart_child();
    }
    with_orca_home(|home| {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("completed_tool_lifecycle_recovers_without_generation_reexecution")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(TOOL_COMPLETION_RESTART_CHILD, "1")
            .env("ORCA_HOME", home)
            .status()
            .expect("start completed tool restart fixture");
        assert!(status.success(), "completed tool restart child failed");
        let thread_id: String = serde_json::from_slice(
            &fs::read(home.join("runtime-surface-tool-completion-restart.json")).unwrap(),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor)).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover completed tool lifecycle",
            )
            .unwrap();
        let snapshot = fresh_snapshot(&thread.surface());
        let result = snapshot
            .tools
            .iter()
            .find(|tool| {
                tool.request.tool_call_id == SurfaceToolCallId::try_new("effect-1").unwrap()
            })
            .and_then(|tool| tool.result.as_ref())
            .expect("completed tool recovered from durable surface ledger");
        assert_eq!(
            result.output.as_ref().map(DisplayText::as_str),
            Some("owned output")
        );
        assert!(snapshot.items.iter().any(|item| matches!(
            item,
            SurfaceItem::ToolResultMessage {
                tool_call_id,
                content,
                terminal,
                ..
            } if tool_call_id == &result.tool_call_id
                && content.as_str() == "owned output"
                && terminal == &result.terminal
        )));
        host.shutdown().unwrap();
    });
}

#[test]
fn readonly_results_commit_as_one_atomic_semantic_batch() {
    with_orca_home(|_| {
        let cwd = tempfile::tempdir().unwrap();
        let host =
            RuntimeHost::start_with_executor(Arc::new(ReadonlyBatchCompletionExecutor)).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "runtime-owned readonly batch completion",
            )
            .unwrap();
        let surface = thread.surface();
        let attachment = fresh_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .unwrap();
        let reserved = committed_value(
            attachment
                .client
                .reserve_operation(
                    request_id(),
                    user_turn_intent(&attachment.baseline.snapshot, "read two exact tools"),
                )
                .unwrap(),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_value(
            attachment
                .client
                .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
                .unwrap(),
        );
        assert!(matches!(
            attachment
                .client
                .wait_operation_terminal(request_id(), operation_id)
                .unwrap(),
            WaitOperationTerminalResult::Terminal { .. }
        ));

        let deadline = Instant::now() + TEST_TIMEOUT;
        let completion_batch = loop {
            if let Some(SurfaceSubscriptionItem::Batch { batch }) = subscription.try_recv() {
                let completed = batch
                    .events
                    .as_slice()
                    .iter()
                    .filter(|event| {
                        matches!(
                            &event.event,
                            SurfaceEvent::Tool(ToolPatch::Completed { .. })
                        )
                    })
                    .count();
                if completed == 2 {
                    break batch;
                }
            }
            assert!(
                Instant::now() < deadline,
                "readonly completion batch was not published"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            completion_batch
                .events
                .as_slice()
                .iter()
                .filter(|event| matches!(
                    &event.event,
                    SurfaceEvent::Item(ItemPatch::Added {
                        item: SurfaceItem::ToolResultMessage { .. },
                    })
                ))
                .count(),
            2
        );
        let snapshot = fresh_snapshot(&surface);
        assert_eq!(
            snapshot
                .tools
                .iter()
                .filter(|tool| tool.result.is_some())
                .count(),
            2
        );
        host.shutdown().unwrap();
    });
}

fn run_tool_completion_restart_child() -> ! {
    let home = PathBuf::from(std::env::var_os("ORCA_HOME").expect("restart child ORCA_HOME"));
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(ToolCompletionExecutor)).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "completed tool restart fixture",
        )
        .unwrap();
    let thread_id = thread.thread_id();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(
                    &attachment.baseline.snapshot,
                    "persist exact tool completion",
                ),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));
    fs::write(
        home.join("runtime-surface-tool-completion-restart.json"),
        serde_json::to_vec(&thread_id).unwrap(),
    )
    .unwrap();
    std::process::exit(0)
}

fn run_assistant_stream_restart_child() -> ! {
    let home = PathBuf::from(std::env::var_os("ORCA_HOME").expect("restart child ORCA_HOME"));
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(BlockingAssistantStreamExecutor)).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "partial assistant restart fixture",
        )
        .unwrap();
    let thread_id = thread.thread_id();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(
                    &attachment.baseline.snapshot,
                    "persist partial assistant stream",
                ),
            )
            .unwrap(),
    );
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), reserved.operation_id, reserved.lease.lease_id)
            .unwrap(),
    );
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        let snapshot = fresh_snapshot(&surface);
        let message_is_durable = snapshot
            .assistant_streams
            .iter()
            .any(|stream| stream.text.as_str() == "restart-partial");
        let secret_is_redacted = snapshot.assistant_streams.iter().any(|stream| {
            stream.channel == AssistantChannel::Reasoning && stream.text.as_str() == "<redacted> "
        });
        if message_is_durable && secret_is_redacted {
            assert!(
                snapshot
                    .assistant_streams
                    .iter()
                    .all(|stream| !stream.text.as_str().contains("opaquevalue1234567890")),
                "partial secret must never enter the durable snapshot"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "partial assistant stream was not durably visible"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::write(
        home.join("runtime-surface-assistant-stream-restart.json"),
        serde_json::to_vec(&thread_id).unwrap(),
    )
    .unwrap();
    std::process::exit(0)
}

#[test]
fn native_permission_allow_cannot_widen_requested_profile() {
    let cwd = tempfile::tempdir().unwrap();
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(PermissionExecutor {
        response_tx,
        tool: permission_tool_request(),
    }))
    .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned permission request",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("claim subscription once");
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "grant exact permission"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );

    let interaction = collect_effect_interaction(
        &mut subscription,
        SurfaceInteractionKind::PermissionRequest,
        "permission-1",
    );
    let requested = match &interaction.request {
        SurfaceInteractionRequest::PermissionRequest { permissions, .. } => permissions.clone(),
        _ => panic!("expected permission request"),
    };
    assert!(response_rx.try_recv().is_err());

    let widened = SurfacePermissionProfile {
        file_system: Some(SurfaceFileSystemPermissionProfile {
            read: None,
            write: Some(vec![
                SurfacePermissionPathLabel(DisplayText::new("/workspace/output")),
                SurfacePermissionPathLabel(DisplayText::new("/workspace/secret")),
            ]),
        }),
        network: None,
        shell: None,
    };
    let rejected = attachment
        .client
        .respond_interaction_by_id(
            request_id(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::PermissionRequest {
                decision: SurfacePermissionClientDecision::Allow {
                    scope: PermissionGrantScope::Turn,
                    permissions: widened,
                    strict_auto_review: false,
                },
            },
        )
        .unwrap();
    assert!(matches!(
        rejected,
        MutationReply::Uncommitted {
            mutation: UncommittedMutation::Invalid { ref error, .. },
        } if error.error().code == SurfaceMutationErrorCode::InvalidInput
    ));
    assert!(response_rx.try_recv().is_err());

    let session_rejected = attachment
        .client
        .respond_interaction_by_id(
            request_id(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::PermissionRequest {
                decision: SurfacePermissionClientDecision::Allow {
                    scope: PermissionGrantScope::Session,
                    permissions: requested.clone(),
                    strict_auto_review: false,
                },
            },
        )
        .unwrap();
    assert!(matches!(
        session_rejected,
        MutationReply::Uncommitted {
            mutation: UncommittedMutation::Invalid { ref error, .. },
        } if error.error().code == SurfaceMutationErrorCode::InvalidInput
    ));
    assert!(response_rx.try_recv().is_err());

    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                interaction.interaction_id,
                SurfaceClientInteractionAnswer::PermissionRequest {
                    decision: SurfacePermissionClientDecision::Allow {
                        scope: PermissionGrantScope::Turn,
                        permissions: requested,
                        strict_auto_review: false,
                    },
                },
            )
            .unwrap(),
    );
    let response = response_rx.recv_timeout(TEST_TIMEOUT).unwrap();
    assert_eq!(response.decision, PermissionResponseDecision::Allow);
    assert_eq!(response.scope, RuntimePermissionGrantScope::Turn);
    assert_eq!(
        response
            .permissions
            .file_system
            .and_then(|profile| profile.write)
            .unwrap(),
        vec![PathBuf::from("/workspace/output")]
    );
    let terminal = attachment
        .client
        .wait_operation_terminal(request_id(), operation_id)
        .unwrap();
    assert!(matches!(
        terminal,
        WaitOperationTerminalResult::Terminal { .. }
    ));
    host.shutdown().unwrap();
}

#[test]
fn sandbox_permission_is_bound_to_the_exact_effect_tool() {
    let cwd = tempfile::tempdir().unwrap();
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(PermissionExecutor {
        response_tx,
        tool: effect_tool_request(),
    }))
    .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned sandbox permission request",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("claim subscription once");
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(
                    &attachment.baseline.snapshot,
                    "grant exact sandbox permission",
                ),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );

    let interaction = collect_effect_interaction(
        &mut subscription,
        SurfaceInteractionKind::PermissionRequest,
        "effect-1",
    );
    let requested = match &interaction.request {
        SurfaceInteractionRequest::PermissionRequest { permissions, .. } => permissions.clone(),
        _ => panic!("expected permission request"),
    };
    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                interaction.interaction_id,
                SurfaceClientInteractionAnswer::PermissionRequest {
                    decision: SurfacePermissionClientDecision::Allow {
                        scope: PermissionGrantScope::Turn,
                        permissions: requested,
                        strict_auto_review: false,
                    },
                },
            )
            .unwrap(),
    );
    assert_eq!(
        response_rx.recv_timeout(TEST_TIMEOUT).unwrap().decision,
        PermissionResponseDecision::Allow
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap(),
        WaitOperationTerminalResult::Terminal { .. }
    ));
    host.shutdown().unwrap();
}

#[test]
fn cancelling_foreground_interaction_commits_cancelled_before_waking_waiter() {
    let cwd = tempfile::tempdir().unwrap();
    let (answer_tx, answer_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx }))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "cancel runtime-owned user input",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("claim subscription once");
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "cancel while waiting"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _fence = match committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    ) {
        AdmissionOutput::Admitted {
            first_generation, ..
        } => first_generation,
        AdmissionOutput::Queued { .. } => panic!("idle operation was queued"),
    };
    let interaction = collect_requested_interaction(&mut subscription);

    let _cancelled = committed_value(
        attachment
            .client
            .cancel_operation(request_id(), operation_id.clone())
            .expect("cancel typed operation"),
    );
    let cancelled_answer = answer_rx.recv_timeout(Duration::from_millis(250));
    if cancelled_answer.is_err() {
        let _ = attachment.client.respond_interaction_by_id(
            request_id(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::UserInput {
                decision: SurfaceUserInputDecision::Answer(DisplayText::new("cleanup")),
            },
        );
    }
    let _terminal = attachment
        .client
        .wait_operation_terminal(request_id(), operation_id)
        .expect("wait for terminal after cancellation");
    let snapshot = match surface.attach_fresh(FreshAttachRequest {
        request_id: request_id(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("fresh snapshot attach failed"),
    };
    let lifecycle = snapshot
        .interactions
        .iter()
        .find(|candidate| candidate.interaction_id == interaction.interaction_id)
        .map(|candidate| candidate.lifecycle.clone());
    host.shutdown().expect("shutdown runtime host");

    assert_eq!(cancelled_answer.unwrap(), None);
    assert!(matches!(
        lifecycle,
        Some(SurfaceInteractionLifecycle::Cancelled {
            reason: InteractionCancelReason::OperationCancelled {
                reason: CancelReason::User,
            },
        })
    ));
}

#[test]
fn thread_close_commits_interaction_cancellation_before_shutdown_joins_generation() {
    let cwd = tempfile::tempdir().unwrap();
    let (answer_tx, answer_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx }))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "close runtime-owned user input",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("claim subscription once");
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "close while waiting"),
            )
            .unwrap(),
    );
    let _fence = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), reserved.operation_id, reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_requested_interaction(&mut subscription);
    let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
    let shutdown_thread = thread.clone();
    std::thread::spawn(move || {
        let _ = shutdown_tx.send(shutdown_thread.shutdown());
    });

    let answer = answer_rx.recv_timeout(Duration::from_millis(500));
    let cancelled = collect_interaction_cancellation(
        &mut subscription,
        &interaction.interaction_id,
        Duration::from_millis(500),
    );
    let shutdown = shutdown_rx.recv_timeout(Duration::from_millis(500));
    if shutdown.is_ok() {
        host.shutdown().expect("shutdown host after thread close");
    } else {
        std::mem::forget(host);
    }

    assert_eq!(answer.unwrap(), None);
    assert!(matches!(
        cancelled,
        Some(InteractionCancelReason::ThreadClose)
    ));
    assert!(shutdown.unwrap().is_ok());
}

#[test]
fn cold_recovery_cancels_unavailable_interaction_before_failing_operation() {
    if std::env::var_os(INTERACTION_RESTART_CHILD).is_some() {
        run_interaction_restart_child();
    }
    with_orca_home(|home| {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("cold_recovery_cancels_unavailable_interaction_before_failing_operation")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(INTERACTION_RESTART_CHILD, "1")
            .env("ORCA_HOME", home)
            .status()
            .expect("start abrupt interaction owner-loss fixture");
        assert!(status.success(), "interaction restart child failed");
        let (thread_id, interaction_id): (String, SurfaceInteractionId) = serde_json::from_slice(
            &fs::read(home.join("runtime-surface-interaction-restart.json"))
                .expect("read interaction restart identity"),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor)).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover unavailable interaction",
            )
            .expect("resume recorded thread and reconcile interaction");
        let snapshot = match thread.surface().attach_fresh(FreshAttachRequest {
            request_id: request_id(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
            _ => panic!("attach recovered interaction snapshot"),
        };
        let recovered = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == interaction_id)
            .expect("recovered interaction remains visible");
        let terminal = snapshot
            .operation_history
            .iter()
            .find_map(|operation| operation.terminal.as_ref())
            .expect("recovered operation is terminal");
        let lifecycle = recovered.lifecycle.clone();
        let operation_terminal = terminal.terminal.clone();
        host.shutdown().expect("shutdown recovered host");

        assert!(matches!(
            lifecycle,
            SurfaceInteractionLifecycle::Cancelled {
                reason: InteractionCancelReason::CapabilityUnavailable,
            }
        ));
        assert!(
            matches!(
                operation_terminal,
                OperationTerminal::Failed {
                    class: FailureClass::RuntimeInvariant,
                    ..
                }
            ),
            "terminal={operation_terminal:?}"
        );
    });
}

#[test]
fn cold_recovery_fails_operation_when_resolved_live_only_payload_is_lost() {
    if std::env::var_os(RESOLVED_INTERACTION_RESTART_CHILD).is_some() {
        run_resolved_interaction_restart_child();
    }
    with_orca_home(|home| {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("cold_recovery_fails_operation_when_resolved_live_only_payload_is_lost")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(RESOLVED_INTERACTION_RESTART_CHILD, "1")
            .env("ORCA_HOME", home)
            .status()
            .expect("start resolved LiveOnly owner-loss fixture");
        assert!(
            status.success(),
            "resolved interaction restart child failed"
        );
        let (thread_id, interaction_id): (String, SurfaceInteractionId) = serde_json::from_slice(
            &fs::read(home.join("runtime-surface-resolved-interaction-restart.json"))
                .expect("read resolved interaction identity"),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor)).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover lost live-only response",
            )
            .unwrap();
        let snapshot = match thread.surface().attach_fresh(FreshAttachRequest {
            request_id: request_id(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
            _ => panic!("attach recovered snapshot"),
        };
        let interaction = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == interaction_id)
            .unwrap();
        let terminal = snapshot
            .operation_history
            .iter()
            .find_map(|operation| operation.terminal.as_ref())
            .unwrap();
        assert!(matches!(
            interaction.lifecycle,
            SurfaceInteractionLifecycle::Resolved { .. }
        ));
        assert!(
            matches!(
                terminal.terminal,
                OperationTerminal::Failed {
                    class: FailureClass::RuntimeInvariant,
                    ..
                }
            ),
            "terminal={:?}",
            terminal.terminal
        );
        host.shutdown().unwrap();
    });
}

#[test]
fn cold_recovery_rematerializes_provider_tool_before_cancelling_unavailable_approval() {
    if std::env::var_os(EFFECT_APPROVAL_RESTART_CHILD).is_some() {
        run_effect_approval_restart_child();
    }
    with_orca_home(|home| {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(
                "cold_recovery_rematerializes_provider_tool_before_cancelling_unavailable_approval",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(EFFECT_APPROVAL_RESTART_CHILD, "1")
            .env("ORCA_HOME", home)
            .status()
            .expect("start pending effect approval restart fixture");
        assert!(status.success());
        let (thread_id, interaction_id): (String, SurfaceInteractionId) = serde_json::from_slice(
            &fs::read(home.join("runtime-surface-effect-approval-restart.json")).unwrap(),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor)).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover pending effect approval",
            )
            .unwrap();
        let snapshot = fresh_snapshot(&thread.surface());
        let interaction = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == interaction_id)
            .unwrap();
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| {
                tool.request.tool_call_id == SurfaceToolCallId::try_new("effect-1").unwrap()
            })
            .expect("provider tool identity rematerialized before recovery settlement");
        let terminal = snapshot
            .operation_history
            .iter()
            .find_map(|operation| operation.terminal.as_ref())
            .unwrap();
        assert!(tool.request.source_response_id.is_some());
        assert!(matches!(
            interaction.lifecycle,
            SurfaceInteractionLifecycle::Cancelled {
                reason: InteractionCancelReason::CapabilityUnavailable,
            }
        ));
        assert!(
            matches!(
                terminal.terminal,
                OperationTerminal::Failed {
                    class: FailureClass::RuntimeInvariant,
                    ..
                }
            ),
            "terminal={:?}",
            terminal.terminal
        );
        host.shutdown().unwrap();
    });
}

#[test]
fn cold_recovery_fails_closed_after_effect_allow_loses_live_only_waiter() {
    if std::env::var_os(RESOLVED_EFFECT_APPROVAL_RESTART_CHILD).is_some() {
        run_resolved_effect_approval_restart_child();
    }
    with_orca_home(|home| {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("cold_recovery_fails_closed_after_effect_allow_loses_live_only_waiter")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(RESOLVED_EFFECT_APPROVAL_RESTART_CHILD, "1")
            .env("ORCA_HOME", home)
            .status()
            .expect("start resolved effect approval restart fixture");
        assert!(status.success());
        let (thread_id, interaction_id): (String, SurfaceInteractionId) = serde_json::from_slice(
            &fs::read(home.join("runtime-surface-resolved-effect-approval-restart.json")).unwrap(),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor)).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover resolved effect approval",
            )
            .unwrap();
        let snapshot = fresh_snapshot(&thread.surface());
        let interaction = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == interaction_id)
            .unwrap();
        let terminal = snapshot
            .operation_history
            .iter()
            .find_map(|operation| operation.terminal.as_ref())
            .unwrap();
        assert!(matches!(
            interaction.lifecycle,
            SurfaceInteractionLifecycle::Resolved { .. }
        ));
        assert!(
            matches!(
                terminal.terminal,
                OperationTerminal::AbortedByRuntimeRestart { .. }
            ),
            "terminal={:?}",
            terminal.terminal
        );
        host.shutdown().unwrap();
    });
}

#[test]
fn cold_recovery_rematerializes_provider_tool_before_cancelling_unavailable_permission() {
    if std::env::var_os(EFFECT_PERMISSION_RESTART_CHILD).is_some() {
        run_effect_permission_restart_child();
    }
    with_orca_home(|home| {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(
                "cold_recovery_rematerializes_provider_tool_before_cancelling_unavailable_permission",
            )
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(EFFECT_PERMISSION_RESTART_CHILD, "1")
            .env("ORCA_HOME", home)
            .status()
            .expect("start pending effect permission restart fixture");
        assert!(status.success());
        let (thread_id, interaction_id): (String, SurfaceInteractionId) = serde_json::from_slice(
            &fs::read(home.join("runtime-surface-effect-permission-restart.json")).unwrap(),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor)).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover pending effect permission",
            )
            .unwrap();
        let snapshot = fresh_snapshot(&thread.surface());
        let interaction = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == interaction_id)
            .unwrap();
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| {
                tool.request.tool_call_id == SurfaceToolCallId::try_new("permission-1").unwrap()
            })
            .expect("permission tool identity rematerialized before recovery settlement");
        let terminal = snapshot
            .operation_history
            .iter()
            .find_map(|operation| operation.terminal.as_ref())
            .unwrap();
        assert!(tool.request.source_response_id.is_some());
        assert!(matches!(
            interaction.lifecycle,
            SurfaceInteractionLifecycle::Cancelled {
                reason: InteractionCancelReason::CapabilityUnavailable,
            }
        ));
        assert!(
            matches!(
                terminal.terminal,
                OperationTerminal::Failed {
                    class: FailureClass::ExternalEffectAmbiguous,
                    ..
                }
            ),
            "terminal={:?}",
            terminal.terminal
        );
        host.shutdown().unwrap();
    });
}

#[test]
fn cold_recovery_fails_closed_after_permission_allow_loses_live_only_waiter() {
    if std::env::var_os(RESOLVED_EFFECT_PERMISSION_RESTART_CHILD).is_some() {
        run_resolved_effect_permission_restart_child();
    }
    with_orca_home(|home| {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("cold_recovery_fails_closed_after_permission_allow_loses_live_only_waiter")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(RESOLVED_EFFECT_PERMISSION_RESTART_CHILD, "1")
            .env("ORCA_HOME", home)
            .status()
            .expect("start resolved effect permission restart fixture");
        assert!(status.success());
        let (thread_id, interaction_id): (String, SurfaceInteractionId) = serde_json::from_slice(
            &fs::read(home.join("runtime-surface-resolved-effect-permission-restart.json"))
                .unwrap(),
        )
        .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(Arc::new(PanicExecutor)).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Resume(thread_id)),
                "recover resolved effect permission",
            )
            .unwrap();
        let snapshot = fresh_snapshot(&thread.surface());
        let interaction = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == interaction_id)
            .unwrap();
        let terminal = snapshot
            .operation_history
            .iter()
            .find_map(|operation| operation.terminal.as_ref())
            .unwrap();
        assert!(matches!(
            interaction.lifecycle,
            SurfaceInteractionLifecycle::Resolved { .. }
        ));
        assert!(matches!(
            terminal.terminal,
            OperationTerminal::Failed {
                class: FailureClass::ClientCapabilityUnavailable,
                ..
            }
        ));
        host.shutdown().unwrap();
    });
}

fn run_effect_approval_restart_child() -> ! {
    let cwd = tempfile::tempdir().unwrap();
    let (resolution_tx, _resolution_rx) = mpsc::sync_channel(1);
    let host =
        RuntimeHost::start_with_executor(Arc::new(ToolApprovalExecutor { resolution_tx })).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "persist pending effect approval",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "pending effect"),
            )
            .unwrap(),
    );
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), reserved.operation_id, reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_effect_interaction(
        &mut subscription,
        SurfaceInteractionKind::ToolApproval,
        "effect-1",
    );
    let home = PathBuf::from(std::env::var_os("ORCA_HOME").unwrap());
    fs::write(
        home.join("runtime-surface-effect-approval-restart.json"),
        serde_json::to_vec(&(thread.thread_id().to_string(), interaction.interaction_id)).unwrap(),
    )
    .unwrap();
    std::process::exit(0)
}

fn run_resolved_effect_approval_restart_child() -> ! {
    let cwd = tempfile::tempdir().unwrap();
    let (resolution_tx, resolution_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(BlockingResolvedToolApprovalExecutor {
        resolution_tx,
    }))
    .unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "persist resolved effect approval",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "resolved effect"),
            )
            .unwrap(),
    );
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), reserved.operation_id, reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_effect_interaction(
        &mut subscription,
        SurfaceInteractionKind::ToolApproval,
        "effect-1",
    );
    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                interaction.interaction_id.clone(),
                SurfaceClientInteractionAnswer::ToolApproval {
                    decision: SurfaceAllowDeny::Allow,
                },
            )
            .unwrap(),
    );
    assert_eq!(
        resolution_rx.recv_timeout(TEST_TIMEOUT).unwrap().decision,
        ApprovalDecision::Allow
    );
    let home = PathBuf::from(std::env::var_os("ORCA_HOME").unwrap());
    fs::write(
        home.join("runtime-surface-resolved-effect-approval-restart.json"),
        serde_json::to_vec(&(thread.thread_id().to_string(), interaction.interaction_id)).unwrap(),
    )
    .unwrap();
    std::process::exit(0)
}

fn run_effect_permission_restart_child() -> ! {
    let cwd = tempfile::tempdir().unwrap();
    let (response_tx, _response_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(PermissionExecutor {
        response_tx,
        tool: permission_tool_request(),
    }))
    .unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "persist pending effect permission",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "pending permission"),
            )
            .unwrap(),
    );
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), reserved.operation_id, reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_effect_interaction(
        &mut subscription,
        SurfaceInteractionKind::PermissionRequest,
        "permission-1",
    );
    let home = PathBuf::from(std::env::var_os("ORCA_HOME").unwrap());
    fs::write(
        home.join("runtime-surface-effect-permission-restart.json"),
        serde_json::to_vec(&(thread.thread_id().to_string(), interaction.interaction_id)).unwrap(),
    )
    .unwrap();
    std::process::exit(0)
}

fn run_resolved_effect_permission_restart_child() -> ! {
    let cwd = tempfile::tempdir().unwrap();
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(BlockingResolvedPermissionExecutor {
        response_tx,
    }))
    .unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "persist resolved effect permission",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "resolved permission"),
            )
            .unwrap(),
    );
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), reserved.operation_id, reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_effect_interaction(
        &mut subscription,
        SurfaceInteractionKind::PermissionRequest,
        "permission-1",
    );
    let requested = match &interaction.request {
        SurfaceInteractionRequest::PermissionRequest { permissions, .. } => permissions.clone(),
        _ => unreachable!(),
    };
    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                interaction.interaction_id.clone(),
                SurfaceClientInteractionAnswer::PermissionRequest {
                    decision: SurfacePermissionClientDecision::Allow {
                        scope: PermissionGrantScope::Turn,
                        permissions: requested,
                        strict_auto_review: false,
                    },
                },
            )
            .unwrap(),
    );
    assert_eq!(
        response_rx.recv_timeout(TEST_TIMEOUT).unwrap().decision,
        PermissionResponseDecision::Allow
    );
    let home = PathBuf::from(std::env::var_os("ORCA_HOME").unwrap());
    fs::write(
        home.join("runtime-surface-resolved-effect-permission-restart.json"),
        serde_json::to_vec(&(thread.thread_id().to_string(), interaction.interaction_id)).unwrap(),
    )
    .unwrap();
    std::process::exit(0)
}

fn run_resolved_interaction_restart_child() -> ! {
    let cwd = tempfile::tempdir().unwrap();
    let (answer_tx, answer_rx) = mpsc::sync_channel(1);
    let host =
        RuntimeHost::start_with_executor(Arc::new(BlockingResolvedUserInputExecutor { answer_tx }))
            .unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "persist resolved LiveOnly interaction",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "resolve then crash"),
            )
            .unwrap(),
    );
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), reserved.operation_id, reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_requested_interaction(&mut subscription);
    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                interaction.interaction_id.clone(),
                SurfaceClientInteractionAnswer::UserInput {
                    decision: SurfaceUserInputDecision::Answer(DisplayText::new("private")),
                },
            )
            .unwrap(),
    );
    assert_eq!(
        answer_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
        Some("private".to_string())
    );
    let home = PathBuf::from(std::env::var_os("ORCA_HOME").unwrap());
    fs::write(
        home.join("runtime-surface-resolved-interaction-restart.json"),
        serde_json::to_vec(&(thread.thread_id().to_string(), interaction.interaction_id)).unwrap(),
    )
    .unwrap();
    std::process::exit(0)
}

fn run_interaction_restart_child() -> ! {
    let cwd = tempfile::tempdir().unwrap();
    let (answer_tx, _answer_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx })).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "persist unavailable interaction",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "persist interaction"),
            )
            .unwrap(),
    );
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), reserved.operation_id, reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_requested_interaction(&mut subscription);
    let home = PathBuf::from(std::env::var_os("ORCA_HOME").expect("restart child ORCA_HOME"));
    fs::write(
        home.join("runtime-surface-interaction-restart.json"),
        serde_json::to_vec(&(thread.thread_id().to_string(), interaction.interaction_id)).unwrap(),
    )
    .unwrap();
    std::process::exit(0)
}

fn with_orca_home<T>(body: impl FnOnce(&Path) -> T) -> T {
    let _guard = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };
    let result = body(home.path());
    match previous {
        Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
    result
}

#[test]
fn jsonl_submit_reserves_a_durable_surface_operation() {
    let cwd = tempfile::tempdir().unwrap();
    let host = RuntimeHost::start_with_executor(Arc::new(ToolCompletionExecutor))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned JSONL submit",
        )
        .expect("start recorded runtime thread");
    let attachment = fresh_interaction_attachment(&thread.surface());
    let legacy_turn_id = orca_core::thread_identity::TurnId::new();
    let mut intent = user_turn_intent(&attachment.baseline.snapshot, "jsonl submit");
    intent.correlation = OperationIngressCorrelation::JsonlThreadTurn {
        rpc_id_digest: Sha256Digest::new([0x42; 32]),
        legacy_turn_id: LegacyTurnId(DisplayText::new(legacy_turn_id.as_str())),
    };

    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(request_id(), intent)
            .expect("JSONL submit reservation should be accepted"),
    );
    let admitted = committed_value(
        attachment
            .client
            .admit_reserved(
                request_id(),
                reserved.operation_id.clone(),
                reserved.lease.lease_id,
            )
            .expect("JSONL submit admission should be accepted"),
    );
    assert!(matches!(admitted, AdmissionOutput::Admitted { .. }));
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(request_id(), reserved.operation_id.clone())
            .expect("wait JSONL operation terminal"),
        WaitOperationTerminalResult::Terminal { .. }
    ));
    let snapshot = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: request_id(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("fresh JSONL snapshot attach failed"),
    };
    let operation = snapshot
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == reserved.operation_id)
        .expect("JSONL operation history");
    assert_eq!(operation.generations[0].logical_turn_id, legacy_turn_id);
    host.shutdown().unwrap();
}

#[test]
fn foreground_user_input_is_durable_before_typed_response_wakes_generation() {
    let cwd = tempfile::tempdir().unwrap();
    let (answer_tx, answer_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx }))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned user input",
        )
        .expect("start recorded runtime thread");
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .expect("claim subscription once");
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "ask before continuing"),
            )
            .expect("reserve foreground operation"),
    );
    let fence = match committed_value(
        attachment
            .client
            .admit_reserved(
                request_id(),
                reserved.operation_id.clone(),
                reserved.lease.lease_id,
            )
            .expect("admit foreground operation"),
    ) {
        AdmissionOutput::Admitted {
            first_generation, ..
        } => first_generation,
        AdmissionOutput::Queued { .. } => panic!("idle operation was queued"),
    };

    let interaction = collect_requested_interaction(&mut subscription);
    assert_eq!(interaction.fence, fence);
    assert_eq!(interaction.kind, SurfaceInteractionKind::UserInput);
    assert_eq!(
        interaction.revision,
        InteractionRevision::try_new(1).unwrap()
    );
    assert!(matches!(
        interaction.route,
        SurfaceInteractionRoute::Exclusive {
            ref attachment_id,
            ..
        } if attachment_id == &attachment.attachment_id
    ));
    assert!(matches!(
        interaction.lifecycle,
        SurfaceInteractionLifecycle::Requested
    ));
    assert!(matches!(
        interaction.recovery_disposition,
        InteractionUnavailableDisposition::RestartableUserInput { .. }
    ));
    assert!(answer_rx.try_recv().is_err());

    let oversized = attachment.client.respond_interaction_by_id(
        request_id(),
        interaction.interaction_id.clone(),
        SurfaceClientInteractionAnswer::UserInput {
            decision: SurfaceUserInputDecision::Answer(DisplayText::new(
                "x".repeat(SURFACE_COMMIT_BATCH_BYTE_LIMIT as usize + 1),
            )),
        },
    );
    assert!(matches!(
        oversized,
        Ok(MutationReply::Uncommitted {
            mutation: UncommittedMutation::Invalid { ref error, .. },
        }) if error.error().code == SurfaceMutationErrorCode::InvalidInput
    ));
    assert!(answer_rx.try_recv().is_err());
    assert!(matches!(
        snapshot_interaction(&surface, &interaction.interaction_id).lifecycle,
        SurfaceInteractionLifecycle::Requested
    ));

    let wrong_kind = attachment.client.respond_interaction_by_id(
        request_id(),
        interaction.interaction_id.clone(),
        SurfaceClientInteractionAnswer::ToolApproval {
            decision: SurfaceAllowDeny::Deny,
        },
    );
    let wrong_kind_code = match wrong_kind {
        Ok(MutationReply::Uncommitted {
            mutation: UncommittedMutation::Invalid { error, .. },
        }) => Some(error.error().code),
        _ => None,
    };
    assert!(answer_rx.try_recv().is_err());

    let output = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                interaction.interaction_id.clone(),
                SurfaceClientInteractionAnswer::UserInput {
                    decision: SurfaceUserInputDecision::Answer(DisplayText::new("ship")),
                },
            )
            .expect("respond through attachment-bound interaction grant"),
    );
    assert_eq!(output.interaction_id, interaction.interaction_id);
    match output.disposition {
        RespondInteractionDisposition::Resolved { .. } => {}
        RespondInteractionDisposition::AlreadyResolved { .. } => {
            panic!("first valid response unexpectedly replayed")
        }
    }
    assert_eq!(
        answer_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
        Some("ship".to_string())
    );
    let late_response = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                interaction.interaction_id.clone(),
                SurfaceClientInteractionAnswer::UserInput {
                    decision: SurfaceUserInputDecision::Answer(DisplayText::new("replace")),
                },
            )
            .expect("replay resolved user input"),
    );
    assert!(matches!(
        late_response.disposition,
        RespondInteractionDisposition::AlreadyResolved { .. }
    ));
    let terminal = thread.surface().attach_fresh(FreshAttachRequest {
        request_id: request_id(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    });
    let snapshot = match terminal {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("fresh terminal snapshot attach failed"),
    };
    let resolved = snapshot
        .interactions
        .iter()
        .find(|candidate| candidate.interaction_id == interaction.interaction_id)
        .expect("resolved interaction remains visible");
    assert!(matches!(
        resolved.lifecycle,
        SurfaceInteractionLifecycle::Resolved { .. }
    ));
    host.shutdown().expect("shutdown runtime host");
    assert_eq!(
        wrong_kind_code,
        Some(SurfaceMutationErrorCode::WrongInteractionKind),
        "wrong-kind response must remain uncommitted"
    );
}

#[test]
fn foreground_mcp_elicitation_round_trips_through_runtime_broker() {
    let cwd = tempfile::tempdir().unwrap();
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(McpExecutor { response_tx })).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "runtime-owned MCP elicitation",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "elicit"),
            )
            .unwrap(),
    );
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), reserved.operation_id, reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_requested_interaction(&mut subscription);
    assert_eq!(interaction.kind, SurfaceInteractionKind::McpElicitation);
    assert!(matches!(
        interaction.request,
        SurfaceInteractionRequest::McpElicitation {
            ref server_name,
            ref server_request_id,
            request: SurfaceMcpElicitationRequest::Url { .. },
            ..
        } if server_name.as_str() == "docs" && server_request_id.as_str() == "mcp-1"
    ));
    for content in [deep_surface_data(128), wide_surface_data(20_000)] {
        let rejected = attachment.client.respond_interaction_by_id(
            request_id(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::McpElicitation {
                decision: SurfaceMcpElicitationDecision::Accept { content },
            },
        );
        assert!(matches!(
            rejected,
            Ok(MutationReply::Uncommitted {
                mutation: UncommittedMutation::Invalid { ref error, .. },
            }) if error.error().code == SurfaceMutationErrorCode::InvalidInput
        ));
        assert!(response_rx.try_recv().is_err());
        assert!(matches!(
            snapshot_interaction(&surface, &interaction.interaction_id).lifecycle,
            SurfaceInteractionLifecycle::Requested
        ));
    }
    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                interaction.interaction_id.clone(),
                SurfaceClientInteractionAnswer::McpElicitation {
                    decision: SurfaceMcpElicitationDecision::Decline,
                },
            )
            .unwrap(),
    );
    assert_eq!(
        response_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
        McpElicitationResponse::Decline
    );
    host.shutdown().unwrap();
}

#[test]
fn cross_kind_interactions_can_reuse_an_adapter_opaque_id() {
    let cwd = tempfile::tempdir().unwrap();
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(CrossKindReuseExecutor { result_tx }))
        .expect("start runtime host");
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "cross-kind opaque id reuse",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "reuse across kinds"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    let first = collect_requested_interaction(&mut subscription);
    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                first.interaction_id.clone(),
                SurfaceClientInteractionAnswer::UserInput {
                    decision: SurfaceUserInputDecision::Answer(DisplayText::new("first")),
                },
            )
            .unwrap(),
    );
    let second = collect_requested_interaction(&mut subscription);
    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                second.interaction_id.clone(),
                SurfaceClientInteractionAnswer::McpElicitation {
                    decision: SurfaceMcpElicitationDecision::Decline,
                },
            )
            .unwrap(),
    );
    let result = result_rx.recv_timeout(TEST_TIMEOUT).unwrap();
    let _ = attachment
        .client
        .wait_operation_terminal(request_id(), operation_id)
        .unwrap();
    host.shutdown().unwrap();

    assert_eq!(first.kind, SurfaceInteractionKind::UserInput);
    assert_eq!(second.kind, SurfaceInteractionKind::McpElicitation);
    assert_ne!(first.interaction_id, second.interaction_id);
    assert_eq!(result.0, Some("first".to_string()));
    assert_eq!(result.1, McpElicitationResponse::Decline);
}

#[test]
fn sequential_operations_can_reuse_the_same_kind_adapter_opaque_id() {
    let cwd = tempfile::tempdir().unwrap();
    let (answer_tx, answer_rx) = mpsc::sync_channel(2);
    let host = RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx })).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "same-kind opaque id reuse",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = fresh_interaction_attachment(&surface);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();

    let first_reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "first reuse operation"),
            )
            .unwrap(),
    );
    let first_operation_id = first_reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(
                request_id(),
                first_operation_id.clone(),
                first_reserved.lease.lease_id,
            )
            .unwrap(),
    );
    let first = collect_requested_interaction(&mut subscription);
    let _ = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                first.interaction_id.clone(),
                SurfaceClientInteractionAnswer::UserInput {
                    decision: SurfaceUserInputDecision::Answer(DisplayText::new("first")),
                },
            )
            .unwrap(),
    );
    assert_eq!(
        answer_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
        Some("first".to_string())
    );
    let _ = attachment
        .client
        .wait_operation_terminal(request_id(), first_operation_id)
        .unwrap();

    let second_snapshot = fresh_snapshot(&surface);
    let second_reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&second_snapshot, "second reuse operation"),
            )
            .unwrap(),
    );
    let second_operation_id = second_reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(
                request_id(),
                second_operation_id.clone(),
                second_reserved.lease.lease_id,
            )
            .unwrap(),
    );
    let second = collect_requested_interaction(&mut subscription);
    let late_old_response = committed_value(
        attachment
            .client
            .respond_interaction_by_id(
                request_id(),
                first.interaction_id.clone(),
                SurfaceClientInteractionAnswer::UserInput {
                    decision: SurfaceUserInputDecision::Answer(DisplayText::new("late-old")),
                },
            )
            .expect("replay first resolved interaction"),
    );
    let answer_after_late = answer_rx.recv_timeout(Duration::from_millis(100)).ok();
    let interaction_after_late = snapshot_interaction(&surface, &second.interaction_id);
    let _ = attachment
        .client
        .cancel_operation(request_id(), second_operation_id.clone());
    let _ = attachment
        .client
        .wait_operation_terminal(request_id(), second_operation_id);
    host.shutdown().unwrap();

    assert_ne!(first.interaction_id, second.interaction_id);
    assert_eq!(second.kind, SurfaceInteractionKind::UserInput);
    assert!(matches!(
        late_old_response.disposition,
        RespondInteractionDisposition::AlreadyResolved { .. }
    ));
    assert_eq!(answer_after_late, None);
    assert!(matches!(
        interaction_after_late.lifecycle,
        SurfaceInteractionLifecycle::Requested
    ));
}

#[test]
fn unavailable_responder_is_durable_before_operation_fails_closed() {
    let cwd = tempfile::tempdir().unwrap();
    let (answer_tx, _answer_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx })).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "durable unavailable responder",
        )
        .unwrap();
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: request_id(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("fresh attachment failed"),
    };
    let _subscription = surface
        .claim_subscription(&attachment.subscription)
        .unwrap();
    let reserved = committed_value(
        attachment
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&attachment.baseline.snapshot, "request unavailable input"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        attachment
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    let terminal = attachment
        .client
        .wait_operation_terminal(request_id(), operation_id)
        .unwrap();
    let snapshot = match surface.attach_fresh(FreshAttachRequest {
        request_id: request_id(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("fresh terminal snapshot attach failed"),
    };
    host.shutdown().unwrap();

    assert!(snapshot.interactions.iter().any(|interaction| matches!(
        (&interaction.route, &interaction.lifecycle),
        (
            SurfaceInteractionRoute::Unassigned { .. },
            SurfaceInteractionLifecycle::Cancelled {
                reason: InteractionCancelReason::CapabilityUnavailable,
            }
        )
    )));
    assert!(matches!(
        terminal,
        WaitOperationTerminalResult::Terminal { value }
            if matches!(
                value.terminal,
                OperationTerminal::Failed {
                    class: FailureClass::ClientCapabilityUnavailable,
                    ..
                }
            )
    ));
}

#[test]
fn detaching_exclusive_responder_rotates_route_to_capable_fallback() {
    let cwd = tempfile::tempdir().unwrap();
    let (answer_tx, answer_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx })).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "reroute detached responder",
        )
        .unwrap();
    let surface = thread.surface();
    let origin = fresh_interaction_attachment(&surface);
    let fallback = fresh_interaction_attachment(&surface);
    let mut origin_subscription = surface.claim_subscription(&origin.subscription).unwrap();
    let _fallback_subscription = surface.claim_subscription(&fallback.subscription).unwrap();
    let reserved = committed_value(
        origin
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&origin.baseline.snapshot, "reroute input"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        origin
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_requested_interaction(&mut origin_subscription);
    let detach = surface.detach(
        &origin.client,
        DetachRequest {
            request_id: request_id(),
        },
    );
    let affected = match detach {
        DetachResult::Detached { receipt } => receipt.affected_route_epochs,
        _ => Vec::new(),
    };
    let response = fallback.client.respond_interaction_by_id(
        request_id(),
        interaction.interaction_id.clone(),
        SurfaceClientInteractionAnswer::UserInput {
            decision: SurfaceUserInputDecision::Answer(DisplayText::new("fallback")),
        },
    );
    if !matches!(response, Ok(MutationReply::Committed { .. })) {
        let _ = fallback
            .client
            .cancel_operation(request_id(), operation_id.clone());
    }
    let _ = fallback
        .client
        .wait_operation_terminal(request_id(), operation_id);
    let answer = answer_rx.recv_timeout(Duration::from_millis(250)).ok();
    host.shutdown().unwrap();

    assert_eq!(
        affected,
        vec![(
            interaction.interaction_id,
            ResponseRouteEpoch::try_new(2).unwrap(),
        )]
    );
    assert!(matches!(response, Ok(MutationReply::Committed { .. })));
    assert_eq!(answer, Some(Some("fallback".to_string())));
}

#[test]
fn dropping_exclusive_responder_rotates_route_before_fallback_can_wake_waiter() {
    let cwd = tempfile::tempdir().unwrap();
    let (answer_tx, answer_rx) = mpsc::sync_channel(1);
    let host = RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx })).unwrap();
    let thread = host
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "reroute dropped responder",
        )
        .unwrap();
    let surface = thread.surface();
    let origin = fresh_interaction_attachment(&surface);
    let fallback = fresh_interaction_attachment(&surface);
    let mut origin_subscription = surface.claim_subscription(&origin.subscription).unwrap();
    let mut fallback_subscription = surface.claim_subscription(&fallback.subscription).unwrap();
    let reserved = committed_value(
        origin
            .client
            .reserve_operation(
                request_id(),
                user_turn_intent(&origin.baseline.snapshot, "drop input responder"),
            )
            .unwrap(),
    );
    let operation_id = reserved.operation_id.clone();
    let _ = committed_value(
        origin
            .client
            .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
            .unwrap(),
    );
    let interaction = collect_requested_interaction(&mut origin_subscription);

    drop(origin_subscription);
    let deadline = Instant::now() + TEST_TIMEOUT;
    let rerouted = loop {
        let current = snapshot_interaction(&surface, &interaction.interaction_id);
        if matches!(
            current.route,
            SurfaceInteractionRoute::Exclusive {
                ref attachment_id,
                epoch,
            } if attachment_id == &fallback.attachment_id
                && epoch == ResponseRouteEpoch::try_new(2).unwrap()
        ) {
            break current;
        }
        assert!(
            Instant::now() < deadline,
            "dropped responder was not rerouted"
        );
        std::thread::yield_now();
    };
    assert_eq!(rerouted.revision, InteractionRevision::try_new(2).unwrap());
    assert!(answer_rx.try_recv().is_err());
    let response = fallback.client.respond_interaction_by_id(
        request_id(),
        interaction.interaction_id.clone(),
        SurfaceClientInteractionAnswer::UserInput {
            decision: SurfaceUserInputDecision::Answer(DisplayText::new("fallback")),
        },
    );
    match &response {
        Ok(MutationReply::Committed { .. }) => {}
        Ok(MutationReply::Uncommitted { mutation }) => {
            panic!("fallback response was uncommitted: {mutation:?}")
        }
        Ok(MutationReply::Deferred { .. }) => panic!("fallback response was deferred"),
        Err(error) => panic!("fallback response failed: {error:?}"),
    }
    assert_eq!(
        answer_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
        Some("fallback".to_string())
    );
    let resolved_count = std::iter::from_fn(|| fallback_subscription.try_recv())
        .filter_map(|item| match item {
            SurfaceSubscriptionItem::Batch { batch } => Some(batch),
            SurfaceSubscriptionItem::Gap { .. } | SurfaceSubscriptionItem::Sealed { .. } => None,
        })
        .map(|batch| {
            batch
                .events
                .as_slice()
                .iter()
                .filter(|event| {
                    matches!(
                        &event.event,
                        SurfaceEvent::Interaction(InteractionPatch::Resolved {
                            interaction_id,
                            ..
                        }) if interaction_id == &interaction.interaction_id
                    )
                })
                .count()
        })
        .sum::<usize>();
    assert_eq!(
        resolved_count, 1,
        "fallback answer must commit exactly once"
    );
    let _ = fallback
        .client
        .wait_operation_terminal(request_id(), operation_id)
        .unwrap();
    host.shutdown().unwrap();
}

#[test]
fn detach_route_append_failure_keeps_fallback_transition_retryable() {
    with_orca_home(|home| {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host =
            RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx })).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry failed detach reroute",
            )
            .unwrap();
        let surface = thread.surface();
        let origin = fresh_interaction_attachment(&surface);
        let fallback = fresh_interaction_attachment(&surface);
        let mut origin_subscription = surface.claim_subscription(&origin.subscription).unwrap();
        let _fallback_subscription = surface.claim_subscription(&fallback.subscription).unwrap();
        let reserved = committed_value(
            origin
                .client
                .reserve_operation(
                    request_id(),
                    user_turn_intent(&origin.baseline.snapshot, "retry detach reroute"),
                )
                .unwrap(),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_value(
            origin
                .client
                .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
                .unwrap(),
        );
        let interaction = collect_requested_interaction(&mut origin_subscription);
        let detach_request = DetachRequest {
            request_id: request_id(),
        };
        let ledger = find_only_jsonl(home);
        let backup = ledger.with_extension("jsonl.detach-route-backup");
        fs::rename(&ledger, &backup).unwrap();
        fs::create_dir(&ledger).unwrap();

        let failed = surface.detach(&origin.client, detach_request.clone());
        let waiter_woke_on_failure = answer_rx.try_recv().ok();
        let after_failure = snapshot_interaction(&surface, &interaction.interaction_id);

        fs::remove_dir(&ledger).unwrap();
        fs::rename(&backup, &ledger).unwrap();
        let intervening_response = origin.client.respond_interaction_by_id(
            request_id(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::UserInput {
                decision: SurfaceUserInputDecision::Answer(DisplayText::new("intervening")),
            },
        );
        let fallback_detach = surface.detach(
            &fallback.client,
            DetachRequest {
                request_id: request_id(),
            },
        );
        let intervening_cancel = origin
            .client
            .cancel_operation(request_id(), operation_id.clone());
        let waiter_woke_on_intervening_cancel = answer_rx.try_recv().ok();
        let after_intervening_cancel = snapshot_interaction(&surface, &interaction.interaction_id);
        let retried = surface.detach(&origin.client, detach_request.clone());
        let detached_receipt = match &retried {
            DetachResult::Detached { receipt } => Some(receipt.clone()),
            _ => None,
        };
        let after_retry = snapshot_interaction(&surface, &interaction.interaction_id);
        let replayed = surface.detach(&origin.client, detach_request);
        let response = fallback.client.respond_interaction_by_id(
            request_id(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::UserInput {
                decision: SurfaceUserInputDecision::Answer(DisplayText::new("fallback")),
            },
        );
        if !matches!(response, Ok(MutationReply::Committed { .. })) {
            let _ = fallback
                .client
                .cancel_operation(request_id(), operation_id.clone());
        }
        let answer = answer_rx.recv_timeout(TEST_TIMEOUT).ok();
        let _ = fallback
            .client
            .wait_operation_terminal(request_id(), operation_id);
        host.shutdown().unwrap();

        assert!(matches!(failed, DetachResult::StaleAttachment { .. }));
        assert_eq!(waiter_woke_on_failure, None);
        assert!(matches!(
            intervening_response,
            Err(SurfaceClientCommandError::RuntimeUnavailable)
        ));
        assert!(matches!(
            fallback_detach,
            DetachResult::StaleAttachment { .. }
        ));
        assert!(matches!(
            intervening_cancel,
            Err(SurfaceClientCommandError::RuntimeUnavailable)
        ));
        assert_eq!(waiter_woke_on_intervening_cancel, None);
        assert_eq!(
            after_intervening_cancel.revision,
            InteractionRevision::try_new(1).unwrap()
        );
        assert!(matches!(
            after_intervening_cancel.lifecycle,
            SurfaceInteractionLifecycle::Requested
        ));
        assert_eq!(
            after_failure.revision,
            InteractionRevision::try_new(1).unwrap()
        );
        assert!(matches!(
            after_failure.route,
            SurfaceInteractionRoute::Exclusive {
                epoch,
                ref attachment_id,
            } if epoch == ResponseRouteEpoch::try_new(1).unwrap()
                && attachment_id == &origin.attachment_id
        ));
        let receipt = detached_receipt.expect("retry must complete the detach");
        assert_eq!(
            receipt.affected_route_epochs,
            vec![(
                interaction.interaction_id.clone(),
                ResponseRouteEpoch::try_new(2).unwrap(),
            )]
        );
        assert!(receipt.route_commit_id.is_some());
        assert!(receipt.route_cursor.is_some());
        assert_eq!(
            after_retry.revision,
            InteractionRevision::try_new(2).unwrap()
        );
        assert!(matches!(
            after_retry.route,
            SurfaceInteractionRoute::Exclusive {
                epoch,
                ref attachment_id,
            } if epoch == ResponseRouteEpoch::try_new(2).unwrap()
                && attachment_id == &fallback.attachment_id
        ));
        assert!(matches!(
            replayed,
            DetachResult::AlreadyDetached { receipt: replayed } if replayed == receipt
        ));
        assert!(matches!(response, Ok(MutationReply::Committed { .. })));
        assert_eq!(answer, Some(Some("fallback".to_string())));
    });
}

#[test]
fn detach_cancellation_append_failure_keeps_no_fallback_settlement_retryable() {
    with_orca_home(|home| {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host =
            RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx })).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry failed detach cancellation",
            )
            .unwrap();
        let surface = thread.surface();
        let origin = fresh_interaction_attachment(&surface);
        let mut origin_subscription = surface.claim_subscription(&origin.subscription).unwrap();
        let reserved = committed_value(
            origin
                .client
                .reserve_operation(
                    request_id(),
                    user_turn_intent(&origin.baseline.snapshot, "retry detach cancellation"),
                )
                .unwrap(),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_value(
            origin
                .client
                .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
                .unwrap(),
        );
        let interaction = collect_requested_interaction(&mut origin_subscription);
        let detach_request = DetachRequest {
            request_id: request_id(),
        };
        let ledger = find_only_jsonl(home);
        let backup = ledger.with_extension("jsonl.detach-cancel-backup");
        fs::rename(&ledger, &backup).unwrap();
        fs::create_dir(&ledger).unwrap();

        let failed = surface.detach(&origin.client, detach_request.clone());
        let waiter_woke_on_failure = answer_rx.try_recv().ok();
        let after_failure = snapshot_interaction(&surface, &interaction.interaction_id);

        fs::remove_dir(&ledger).unwrap();
        fs::rename(&backup, &ledger).unwrap();
        let retried = surface.detach(&origin.client, detach_request.clone());
        let detached_receipt = match &retried {
            DetachResult::Detached { receipt } => Some(receipt.clone()),
            _ => None,
        };
        let after_retry = snapshot_interaction(&surface, &interaction.interaction_id);
        let replayed = surface.detach(&origin.client, detach_request);
        let waiter_after_retry = answer_rx.recv_timeout(Duration::from_millis(250)).ok();
        let observer = fresh_control_attachment(&surface);
        if waiter_after_retry.is_none() {
            let _ = observer
                .client
                .cancel_operation(request_id(), operation_id.clone());
        }
        let terminal = observer
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap();
        host.shutdown().unwrap();

        assert!(matches!(failed, DetachResult::StaleAttachment { .. }));
        assert_eq!(waiter_woke_on_failure, None);
        assert_eq!(
            after_failure.revision,
            InteractionRevision::try_new(1).unwrap()
        );
        assert!(matches!(
            after_failure.route,
            SurfaceInteractionRoute::Exclusive {
                epoch,
                ref attachment_id,
            } if epoch == ResponseRouteEpoch::try_new(1).unwrap()
                && attachment_id == &origin.attachment_id
        ));
        let receipt = detached_receipt.expect("retry must complete the detach");
        assert_eq!(
            receipt.affected_route_epochs,
            vec![(
                interaction.interaction_id,
                ResponseRouteEpoch::try_new(2).unwrap(),
            )]
        );
        assert!(receipt.route_commit_id.is_some());
        assert!(receipt.route_cursor.is_some());
        assert_eq!(
            after_retry.revision,
            InteractionRevision::try_new(3).unwrap()
        );
        assert!(matches!(
            after_retry.route,
            SurfaceInteractionRoute::Unassigned { epoch }
                if epoch == ResponseRouteEpoch::try_new(2).unwrap()
        ));
        assert!(matches!(
            after_retry.lifecycle,
            SurfaceInteractionLifecycle::Cancelled {
                reason: InteractionCancelReason::CapabilityUnavailable,
            }
        ));
        assert!(matches!(
            replayed,
            DetachResult::AlreadyDetached { receipt: replayed } if replayed == receipt
        ));
        assert_eq!(waiter_after_retry, Some(None));
        assert!(matches!(
            terminal,
            WaitOperationTerminalResult::Terminal { value }
                if matches!(
                    value.terminal,
                    OperationTerminal::Failed {
                        class: FailureClass::ClientCapabilityUnavailable,
                        ..
                    }
                )
        ));
    });
}

#[test]
fn append_failure_retains_private_first_winner_until_exact_batch_retry() {
    with_orca_home(|home| {
        let cwd = tempfile::tempdir().unwrap();
        let (answer_tx, answer_rx) = mpsc::sync_channel(1);
        let host =
            RuntimeHost::start_with_executor(Arc::new(UserInputExecutor { answer_tx })).unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry private interaction winner",
            )
            .unwrap();
        let surface = thread.surface();
        let attachment = fresh_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .unwrap();
        let reserved = committed_value(
            attachment
                .client
                .reserve_operation(
                    request_id(),
                    user_turn_intent(&attachment.baseline.snapshot, "private winner"),
                )
                .unwrap(),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_value(
            attachment
                .client
                .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
                .unwrap(),
        );
        let interaction = collect_requested_interaction(&mut subscription);
        let ledger = find_only_jsonl(home);
        let backup = ledger.with_extension("jsonl.private-winner-backup");
        fs::rename(&ledger, &backup).unwrap();
        fs::create_dir(&ledger).unwrap();
        let failed = attachment.client.respond_interaction_by_id(
            request_id(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::UserInput {
                decision: SurfaceUserInputDecision::Answer(DisplayText::new("winner")),
            },
        );
        assert!(matches!(
            failed,
            Err(SurfaceClientCommandError::RuntimeUnavailable)
        ));
        assert!(answer_rx.try_recv().is_err());
        fs::remove_dir(&ledger).unwrap();
        fs::rename(&backup, &ledger).unwrap();

        let retried = committed_value(
            attachment
                .client
                .respond_interaction_by_id(
                    request_id(),
                    interaction.interaction_id.clone(),
                    SurfaceClientInteractionAnswer::UserInput {
                        decision: SurfaceUserInputDecision::Answer(DisplayText::new("conflict")),
                    },
                )
                .unwrap(),
        );
        assert!(matches!(
            retried.disposition,
            RespondInteractionDisposition::AlreadyResolved { .. }
        ));
        let late_response = committed_value(
            attachment
                .client
                .respond_interaction_by_id(
                    request_id(),
                    interaction.interaction_id.clone(),
                    SurfaceClientInteractionAnswer::UserInput {
                        decision: SurfaceUserInputDecision::Answer(DisplayText::new("late")),
                    },
                )
                .expect("replay retained private winner"),
        );
        assert!(matches!(
            late_response.disposition,
            RespondInteractionDisposition::AlreadyResolved { .. }
        ));
        assert_eq!(
            answer_rx.recv_timeout(TEST_TIMEOUT).unwrap(),
            Some("winner".to_string())
        );
        let _ = attachment
            .client
            .wait_operation_terminal(request_id(), operation_id)
            .unwrap();
        host.shutdown().unwrap();
    });
}

#[test]
fn tool_approval_allow_wakes_only_after_exact_resolution_batch_commits() {
    with_orca_home(|home| {
        let cwd = tempfile::tempdir().unwrap();
        let (resolution_tx, resolution_rx) = mpsc::sync_channel(1);
        let host =
            RuntimeHost::start_with_executor(Arc::new(ToolApprovalExecutor { resolution_tx }))
                .unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry effect approval winner",
            )
            .unwrap();
        let surface = thread.surface();
        let attachment = fresh_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .unwrap();
        let reserved = committed_value(
            attachment
                .client
                .reserve_operation(
                    request_id(),
                    user_turn_intent(&attachment.baseline.snapshot, "effect approval winner"),
                )
                .unwrap(),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_value(
            attachment
                .client
                .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
                .unwrap(),
        );
        let interaction = collect_effect_interaction(
            &mut subscription,
            SurfaceInteractionKind::ToolApproval,
            "effect-1",
        );
        let ledger = find_only_jsonl(home);
        let backup = ledger.with_extension("jsonl.effect-approval-winner-backup");
        fs::rename(&ledger, &backup).unwrap();
        fs::create_dir(&ledger).unwrap();
        let failed = attachment.client.respond_interaction_by_id(
            request_id(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::ToolApproval {
                decision: SurfaceAllowDeny::Allow,
            },
        );
        assert!(matches!(
            failed,
            Err(SurfaceClientCommandError::RuntimeUnavailable)
        ));
        assert!(resolution_rx.try_recv().is_err());
        fs::remove_dir(&ledger).unwrap();
        fs::rename(&backup, &ledger).unwrap();

        let retried = committed_value(
            attachment
                .client
                .respond_interaction_by_id(
                    request_id(),
                    interaction.interaction_id.clone(),
                    SurfaceClientInteractionAnswer::ToolApproval {
                        decision: SurfaceAllowDeny::Deny,
                    },
                )
                .unwrap(),
        );
        assert!(matches!(
            retried.disposition,
            RespondInteractionDisposition::AlreadyResolved { .. }
        ));
        assert_eq!(
            resolution_rx.recv_timeout(TEST_TIMEOUT).unwrap().decision,
            ApprovalDecision::Allow
        );
        assert!(resolution_rx.try_recv().is_err());
        let replay = committed_value(
            attachment
                .client
                .respond_interaction_by_id(
                    request_id(),
                    interaction.interaction_id,
                    SurfaceClientInteractionAnswer::ToolApproval {
                        decision: SurfaceAllowDeny::Deny,
                    },
                )
                .expect("replay resolved tool approval"),
        );
        assert!(matches!(
            replay.disposition,
            RespondInteractionDisposition::AlreadyResolved { .. }
        ));
        assert!(matches!(
            attachment
                .client
                .wait_operation_terminal(request_id(), operation_id)
                .unwrap(),
            WaitOperationTerminalResult::Terminal { .. }
        ));
        host.shutdown().unwrap();
    });
}

#[test]
fn permission_allow_wakes_only_after_exact_resolution_batch_commits() {
    with_orca_home(|home| {
        let cwd = tempfile::tempdir().unwrap();
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(PermissionExecutor {
            response_tx,
            tool: permission_tool_request(),
        }))
        .unwrap();
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "retry effect permission winner",
            )
            .unwrap();
        let surface = thread.surface();
        let attachment = fresh_interaction_attachment(&surface);
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .unwrap();
        let reserved = committed_value(
            attachment
                .client
                .reserve_operation(
                    request_id(),
                    user_turn_intent(&attachment.baseline.snapshot, "effect permission winner"),
                )
                .unwrap(),
        );
        let operation_id = reserved.operation_id.clone();
        let _ = committed_value(
            attachment
                .client
                .admit_reserved(request_id(), operation_id.clone(), reserved.lease.lease_id)
                .unwrap(),
        );
        let interaction = collect_effect_interaction(
            &mut subscription,
            SurfaceInteractionKind::PermissionRequest,
            "permission-1",
        );
        let requested = match &interaction.request {
            SurfaceInteractionRequest::PermissionRequest { permissions, .. } => permissions.clone(),
            _ => unreachable!(),
        };
        let ledger = find_only_jsonl(home);
        let backup = ledger.with_extension("jsonl.effect-permission-winner-backup");
        fs::rename(&ledger, &backup).unwrap();
        fs::create_dir(&ledger).unwrap();
        let failed = attachment.client.respond_interaction_by_id(
            request_id(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::PermissionRequest {
                decision: SurfacePermissionClientDecision::Allow {
                    scope: PermissionGrantScope::Turn,
                    permissions: requested.clone(),
                    strict_auto_review: false,
                },
            },
        );
        assert!(matches!(
            failed,
            Err(SurfaceClientCommandError::RuntimeUnavailable)
        ));
        assert!(response_rx.try_recv().is_err());
        fs::remove_dir(&ledger).unwrap();
        fs::rename(&backup, &ledger).unwrap();

        let retried = committed_value(
            attachment
                .client
                .respond_interaction_by_id(
                    request_id(),
                    interaction.interaction_id.clone(),
                    SurfaceClientInteractionAnswer::PermissionRequest {
                        decision: SurfacePermissionClientDecision::Deny {
                            scope: PermissionGrantScope::Turn,
                            permissions: requested.clone(),
                            strict_auto_review: false,
                        },
                    },
                )
                .unwrap(),
        );
        assert!(matches!(
            retried.disposition,
            RespondInteractionDisposition::AlreadyResolved { .. }
        ));
        assert_eq!(
            response_rx.recv_timeout(TEST_TIMEOUT).unwrap().decision,
            PermissionResponseDecision::Allow
        );
        assert!(response_rx.try_recv().is_err());
        let replay = committed_value(
            attachment
                .client
                .respond_interaction_by_id(
                    request_id(),
                    interaction.interaction_id,
                    SurfaceClientInteractionAnswer::PermissionRequest {
                        decision: SurfacePermissionClientDecision::Deny {
                            scope: PermissionGrantScope::Turn,
                            permissions: requested,
                            strict_auto_review: false,
                        },
                    },
                )
                .expect("replay resolved permission response"),
        );
        assert!(matches!(
            replay.disposition,
            RespondInteractionDisposition::AlreadyResolved { .. }
        ));
        assert!(matches!(
            attachment
                .client
                .wait_operation_terminal(request_id(), operation_id)
                .unwrap(),
            WaitOperationTerminalResult::Terminal { .. }
        ));
        host.shutdown().unwrap();
    });
}

fn find_only_jsonl(root: &Path) -> PathBuf {
    fn visit(directory: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, found);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    visit(root, &mut found);
    assert_eq!(found.len(), 1, "expected one recorded session ledger");
    found.pop().unwrap()
}

fn snapshot_interaction(
    surface: &RuntimeSurfaceHandle,
    interaction_id: &SurfaceInteractionId,
) -> SurfaceInteractionView {
    let snapshot = fresh_snapshot(surface);
    snapshot
        .interactions
        .iter()
        .find(|interaction| &interaction.interaction_id == interaction_id)
        .cloned()
        .expect("interaction remains visible")
}

fn deep_surface_data(depth: usize) -> SurfaceDataValue {
    (0..depth).fold(SurfaceDataValue::Null, |value, _| {
        SurfaceDataValue::Array(vec![value])
    })
}

fn wide_surface_data(nodes: usize) -> SurfaceDataValue {
    SurfaceDataValue::Array(vec![SurfaceDataValue::Null; nodes])
}

fn fresh_snapshot(surface: &RuntimeSurfaceHandle) -> Arc<SurfaceSnapshot> {
    match surface.attach_fresh(FreshAttachRequest {
        request_id: request_id(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("fresh interaction snapshot attach failed"),
    }
}

fn collect_requested_interaction(
    receiver: &mut SurfaceSubscriptionReceiver,
) -> SurfaceInteractionView {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        while let Some(item) = receiver.try_recv() {
            if let SurfaceSubscriptionItem::Batch { batch } = item {
                for event in batch.events.as_slice() {
                    if let SurfaceEvent::Interaction(InteractionPatch::Requested { interaction }) =
                        &event.event
                    {
                        return interaction.clone();
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "durable interaction request was not published"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn collect_effect_interaction(
    receiver: &mut SurfaceSubscriptionReceiver,
    kind: SurfaceInteractionKind,
    tool_call_id: &str,
) -> SurfaceInteractionView {
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut semantic_committed = false;
    loop {
        while let Some(item) = receiver.try_recv() {
            if let SurfaceSubscriptionItem::Batch { batch } = item {
                let mut response = None;
                let mut requested_tool = None;
                let mut interaction = None;
                for (index, event) in batch.events.as_slice().iter().enumerate() {
                    match &event.event {
                        SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted {
                            response: completed,
                        }) => response = Some((index, completed.response_id.clone())),
                        SurfaceEvent::Tool(ToolPatch::Requested { request })
                            if request.tool_call_id
                                == SurfaceToolCallId::try_new(tool_call_id).unwrap() =>
                        {
                            requested_tool = Some((index, request.source_response_id.clone()));
                        }
                        SurfaceEvent::Interaction(InteractionPatch::Requested {
                            interaction: requested,
                        }) if requested.kind == kind => interaction = Some(requested.clone()),
                        _ => {}
                    }
                }
                if response.is_some() || requested_tool.is_some() {
                    let (response_index, response_id) =
                        response.expect("response completion and tool request must be atomic");
                    let (tool_index, source_response_id) = requested_tool
                        .expect("response completion and tool request must be atomic");
                    assert!(response_index < tool_index);
                    assert_eq!(source_response_id.as_ref(), Some(&response_id));
                    semantic_committed = true;
                }
                if let Some(interaction) = interaction {
                    assert!(
                        semantic_committed,
                        "effect-bearing interaction published before durable tool identity"
                    );
                    return interaction;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "effect-bearing interaction was not published"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn collect_interaction_cancellation(
    receiver: &mut SurfaceSubscriptionReceiver,
    interaction_id: &SurfaceInteractionId,
    timeout: Duration,
) -> Option<InteractionCancelReason> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(item) = receiver.try_recv() {
            if let SurfaceSubscriptionItem::Batch { batch } = item {
                for event in batch.events.as_slice() {
                    if let SurfaceEvent::Interaction(InteractionPatch::Cancelled {
                        interaction_id: candidate,
                        reason,
                        ..
                    }) = &event.event
                        && candidate == interaction_id
                    {
                        return Some(reason.clone());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

fn fresh_interaction_attachment(surface: &RuntimeSurfaceHandle) -> FreshSurfaceAttachment {
    match surface.attach_fresh(FreshAttachRequest {
        request_id: request_id(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::RespondGrantedInteraction,
        ]),
        interaction_capabilities: BTreeSet::from([
            SurfaceInteractionKind::ToolApproval,
            SurfaceInteractionKind::PermissionRequest,
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionKind::McpElicitation,
        ]),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("fresh interaction attachment failed"),
    }
}

fn fresh_control_attachment(surface: &RuntimeSurfaceHandle) -> FreshSurfaceAttachment {
    match surface.attach_fresh(FreshAttachRequest {
        request_id: request_id(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("fresh control attachment failed"),
    }
}

fn user_turn_intent(snapshot: &SurfaceSnapshot, text: &str) -> OperationRequestIntent {
    OperationRequestIntent {
        correlation: OperationIngressCorrelation::TuiUser,
        kind: OperationKind::UserTurn,
        input: Some(SurfaceInputRequest {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                text: DisplayText::new(text),
            }])
            .unwrap(),
        }),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: snapshot.settings.thread_revision,
            expected_policy_epoch: snapshot.settings.effective.policy_epoch,
        },
    }
}

fn committed_value<T>(reply: MutationReply<T>) -> T {
    match reply {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred { .. } => panic!("mutation was deferred"),
        MutationReply::Uncommitted { .. } => panic!("mutation was not committed"),
    }
}

fn request_id() -> SurfaceRequestId {
    SurfaceRequestId::try_from_bytes(uuid_v7_bytes(NEXT_ID.fetch_add(1, Ordering::Relaxed)))
        .unwrap()
}

fn uuid_v7_bytes(value: u64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&value.to_be_bytes());
    bytes[8..].copy_from_slice(&value.rotate_left(17).to_be_bytes());
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    bytes
}

fn test_config(cwd: PathBuf, history_mode: HistoryMode) -> RunConfig {
    RunConfig {
        app_version: "test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::Suggest,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::parse(None).unwrap(),
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: HashMap::new(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        budget: Default::default(),
        subagents: SubagentConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::default(),
        vim_mode: false,
        vim_insert_escape: None,
        update_check: false,
        desktop_notifications: false,
        terminal_notifications: false,
        auto_memory: false,
    }
}
