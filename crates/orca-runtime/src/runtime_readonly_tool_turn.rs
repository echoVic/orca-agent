use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types::{ToolOutputTruncation, ToolRequest, ToolResult, ToolStatus};
use orca_mcp::McpRegistry;

use crate::hooks::{HookContext, HookRunError, HookRunner};
use crate::runtime_surface::RuntimeProviderResponseIngress;
use crate::runtime_tool_call::{RuntimeReadonlyToolInvocation, RuntimeToolCallRuntime};
use crate::session::record_tool_result_for_agent;
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_execution::semantic_commit_failure;
use crate::tool_invocation::{
    apply_pre_tool_outcome_with_external, prepare_tool_invocation_with_external,
};
use crate::tool_turn::{ToolTurnOutcome, record_root_task_output_truncation};

pub(crate) struct RuntimeReadonlyBatchContext<'a, W: io::Write> {
    pub(crate) cwd: &'a Path,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) tool_requests: &'a [ToolRequest],
    pub(crate) emit_deltas: bool,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) output_truncation: ToolOutputTruncation,
    pub(crate) max_parallel: usize,
    pub(crate) provider_response_ingress: Option<&'a dyn RuntimeProviderResponseIngress>,
}

pub(crate) struct RuntimeReadonlyToolTurnContext<'a, W: io::Write> {
    pub(crate) request: RuntimeReadonlyToolTurnRequest<'a>,
    pub(crate) io: RuntimeReadonlyToolTurnIo<'a, W>,
    pub(crate) services: RuntimeReadonlyToolTurnServices<'a>,
}

pub(crate) struct RuntimeReadonlyToolTurnRequest<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) tool_requests: &'a [ToolRequest],
    pub(crate) emit_deltas: bool,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) output_truncation: ToolOutputTruncation,
    pub(crate) max_parallel: usize,
}

pub(crate) struct RuntimeReadonlyToolTurnIo<'a, W: io::Write> {
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
}

pub(crate) struct RuntimeReadonlyToolTurnServices<'a> {
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) provider_response_ingress: Option<&'a dyn RuntimeProviderResponseIngress>,
    pub(crate) task_registry: Option<&'a TaskRegistry>,
    pub(crate) root_task_id: Option<&'a str>,
}

pub(crate) struct RuntimeReadonlyBatchExecution {
    pub(crate) results: Vec<ToolResult>,
    pub(crate) event_error: Option<io::Error>,
}

fn retain_first_event_error(slot: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result
        && slot.is_none()
    {
        *slot = Some(error);
    }
}

pub(crate) fn execute_readonly_batch<W: io::Write>(
    context: RuntimeReadonlyBatchContext<'_, W>,
) -> io::Result<RuntimeReadonlyBatchExecution> {
    let RuntimeReadonlyBatchContext {
        cwd,
        events,
        sink,
        tool_requests,
        emit_deltas,
        mcp_registry,
        hooks,
        cancel,
        output_truncation,
        max_parallel,
        provider_response_ingress,
    } = context;
    let mut early_results: Vec<Option<ToolResult>> = vec![None; tool_requests.len()];
    let mut runnable = Vec::new();
    let mut event_error = None;

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        if emit_deltas {
            retain_first_event_error(
                &mut event_error,
                sink.emit(events.tool_call_requested(tool_request)),
            );
        }
        if cancel.is_cancelled() {
            early_results[idx] = Some(ToolResult::cancelled_before_start(
                tool_request,
                "the read-only batch was cancelled before dispatch",
            ));
            continue;
        }
        let invocation =
            prepare_tool_invocation_with_external(tool_request, 0, u32::MAX, mcp_registry, &[]);
        match hooks.run_with_cancel_result(
            HookEvent::PreToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
            cancel,
        ) {
            Ok(outcome) => {
                match apply_pre_tool_outcome_with_external(invocation, &outcome, mcp_registry, &[])
                {
                    Ok(invocation) => runnable.push((idx, invocation.effective)),
                    Err(error) => early_results[idx] = Some(error.into_result()),
                }
            }
            Err(error) => {
                early_results[idx] = Some(match error {
                    HookRunError::Cancelled(_) => ToolResult::cancelled_before_start(
                        tool_request,
                        "the pre-tool hook was cancelled",
                    ),
                    HookRunError::Failed(error) => ToolResult::failed_before_start(
                        tool_request,
                        format!("pre_tool_use hook blocked tool: {error}"),
                        None,
                    ),
                });
            }
        }
    }

    let runnable_requests = runnable
        .iter()
        .map(|(_, request)| request.clone())
        .collect::<Vec<_>>();
    let runnable_results = if runnable_requests.is_empty() {
        Vec::new()
    } else {
        RuntimeToolCallRuntime::for_current_execution()?.execute_readonly_batch(
            runnable_requests
                .iter()
                .cloned()
                .map(|request| RuntimeReadonlyToolInvocation {
                    request,
                    cwd: cwd.to_path_buf(),
                    mcp_registry: mcp_registry.clone(),
                    output_truncation,
                })
                .collect(),
            max_parallel,
            cancel,
        )
    };

    let mut results = early_results;
    for ((original_idx, _), result) in runnable.into_iter().zip(runnable_results) {
        results[original_idx] = Some(result);
    }
    let results = results
        .into_iter()
        .map(|result| result.expect("each read-only batch item has a result"))
        .collect::<Vec<_>>();

    if let Some(ingress) = provider_response_ingress {
        ingress
            .commit_tool_results(&results)
            .map_err(semantic_commit_failure)?;
    }

    for (tool_request, result) in tool_requests.iter().zip(results.iter()) {
        if emit_deltas {
            retain_first_event_error(
                &mut event_error,
                sink.emit(events.tool_call_completed(result)),
            );
            if let Err(error) = hooks.run(
                HookEvent::PostToolUse,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: Some(tool_request),
                    tool_result: Some(result),
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            ) {
                retain_first_event_error(
                    &mut event_error,
                    sink.emit(events.error(&format!("post_tool_use hook failed: {error}"))),
                );
            }
        }
    }

    Ok(RuntimeReadonlyBatchExecution {
        results,
        event_error,
    })
}

pub(crate) fn should_run_readonly_batch(
    max_read_parallel: usize,
    tool_request: &ToolRequest,
) -> bool {
    !is_goal_control_tool(tool_request)
        && orca_tools::should_run_readonly_batch(max_read_parallel, tool_request)
}

pub(crate) fn collect_readonly_batch(
    max_read_parallel: usize,
    tool_requests: &[ToolRequest],
    start: usize,
) -> usize {
    let end = orca_tools::collect_readonly_batch(max_read_parallel, tool_requests, start);
    tool_requests[start..end]
        .iter()
        .position(is_goal_control_tool)
        .map(|offset| start + offset)
        .unwrap_or(end)
}

fn is_goal_control_tool(request: &ToolRequest) -> bool {
    matches!(
        request.name,
        orca_core::tool_types::ToolName::GetGoal
            | orca_core::tool_types::ToolName::CreateGoal
            | orca_core::tool_types::ToolName::UpdateGoal
    )
}

pub(crate) fn record_readonly_batch_results(
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    results: Vec<ToolResult>,
    emit_deltas: bool,
) -> io::Result<()> {
    let mut record_error = None;
    for result in results {
        if let Err(error) = record_tool_result_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            &result,
            emit_deltas,
        ) && record_error.is_none()
        {
            record_error = Some(error);
        }
    }
    match record_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(crate) fn run_readonly_tool_turn<W: io::Write>(
    context: RuntimeReadonlyToolTurnContext<'_, W>,
) -> io::Result<ToolTurnOutcome> {
    let RuntimeReadonlyToolTurnContext {
        request,
        io,
        services,
    } = context;
    let RuntimeReadonlyToolTurnRequest {
        cwd,
        tool_requests,
        emit_deltas,
        cancel,
        output_truncation,
        max_parallel,
    } = request;
    let RuntimeReadonlyToolTurnIo {
        events,
        sink,
        conversation,
        history_writer,
    } = io;
    let RuntimeReadonlyToolTurnServices {
        mcp_registry,
        hooks,
        provider_response_ingress,
        task_registry,
        root_task_id,
    } = services;
    let execution = execute_readonly_batch(RuntimeReadonlyBatchContext {
        cwd,
        events,
        sink,
        tool_requests,
        emit_deltas,
        mcp_registry,
        hooks,
        cancel,
        output_truncation,
        max_parallel,
        provider_response_ingress,
    })?;
    let cancelled_result = execution
        .results
        .iter()
        .find(|result| result.status == ToolStatus::Cancelled);
    let cancelled_error = cancelled_result.and_then(|result| result.error.clone());
    let turn_cancelled = cancel.is_cancelled() || cancelled_result.is_some();
    let output_truncated = execution.results.iter().any(|result| result.truncated);

    record_readonly_batch_results(conversation, history_writer, execution.results, emit_deltas)?;
    if let Some(task_registry) = task_registry {
        record_root_task_output_truncation(
            task_registry,
            root_task_id,
            output_truncated,
            events,
            sink,
        )?;
    }
    if let Some(error) = execution.event_error {
        return Err(error);
    }
    if turn_cancelled {
        return Ok(ToolTurnOutcome::Return {
            status: RunStatus::Cancelled,
            error: cancelled_error.or_else(|| Some("read-only tool turn cancelled".to_string())),
            terminal: None,
        });
    }
    Ok(ToolTurnOutcome::Continue)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;

    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Message;
    use orca_core::event_schema::{EventEnvelope, EventType};
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolInvocationStarted, ToolName, ToolStatus};
    use orca_mcp::transport::McpTransport;
    use serde_json::{Value, json};

    use crate::model_response::RuntimeModelResponse;
    use crate::runtime_host::{
        GenerationContext, HostedTurnRequest, InterruptOperationResult, OperationOutcome,
        RuntimeHost, RuntimeHostError, ThreadOperationExecutor, ThreadOperationOutcome,
    };
    use crate::thread::RuntimeThread;

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingReadonlyBatchIngress {
        batches: Mutex<Vec<Vec<String>>>,
    }

    impl RuntimeProviderResponseIngress for RecordingReadonlyBatchIngress {
        fn commit_response(&self, _response: &RuntimeModelResponse) -> io::Result<()> {
            Ok(())
        }

        fn commit_provider_step(
            &self,
            _identity: &orca_core::thread_item_projection::ModelResponseIdentity,
            _step: &orca_core::provider_types::ProviderStep,
        ) -> io::Result<()> {
            Ok(())
        }

        fn commit_tool_results(&self, results: &[ToolResult]) -> io::Result<()> {
            self.batches
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(results.iter().map(|result| result.id.clone()).collect());
            Ok(())
        }
    }

    #[test]
    fn readonly_execution_commits_all_results_in_one_semantic_batch() {
        let requests = [
            ToolRequest {
                id: "readonly-1".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("one.txt".to_string()),
                raw_arguments: Some(r#"{"path":"one.txt"}"#.to_string()),
            },
            ToolRequest {
                id: "readonly-2".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("two.txt".to_string()),
                raw_arguments: Some(r#"{"path":"two.txt"}"#.to_string()),
            },
        ];
        let cancel = CancelToken::new();
        cancel.cancel();
        let ingress = RecordingReadonlyBatchIngress::default();
        let mut events = EventFactory::new("readonly-semantic-batch".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let execution = execute_readonly_batch(RuntimeReadonlyBatchContext {
            cwd: Path::new("."),
            events: &mut events,
            sink: &mut sink,
            tool_requests: &requests,
            emit_deltas: false,
            mcp_registry: &registry,
            hooks: &hooks,
            cancel: &cancel,
            output_truncation: ToolOutputTruncation::default(),
            max_parallel: 2,
            provider_response_ingress: Some(&ingress),
        })
        .unwrap();

        assert_eq!(execution.results.len(), 2);
        assert_eq!(
            *ingress
                .batches
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            vec![vec!["readonly-1".to_string(), "readonly-2".to_string()]]
        );
    }

    struct BlockingResourceTransport {
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl McpTransport for BlockingResourceTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(json!({"capabilities": {"resources": {}}}))
        }

        fn list_tools(&self) -> Result<Value, String> {
            Ok(json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("blocking resource transport does not support tools".to_string())
        }

        fn list_resources(&self) -> Result<Value, String> {
            self.started.send(()).map_err(|error| error.to_string())?;
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap_or_else(|error| error.into_inner());
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(|error| error.into_inner());
            }
            Ok(json!({"resources": []}))
        }

        fn list_resources_or_cancel(
            &self,
            should_cancel: &dyn Fn() -> bool,
        ) -> Result<Value, String> {
            self.started.send(()).map_err(|error| error.to_string())?;
            while !should_cancel() {
                thread::sleep(Duration::from_millis(5));
            }
            Err("MCP tool call cancelled".to_string())
        }

        fn list_resource_templates(&self) -> Result<Value, String> {
            Ok(json!({"resourceTemplates": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("blocking resource transport does not support reads".to_string())
        }
    }

    #[derive(Clone, Default)]
    struct HostCleanupGate {
        state: Arc<(Mutex<HostCleanupState>, Condvar)>,
    }

    #[derive(Default)]
    struct HostCleanupState {
        entered: bool,
        cancel_seen: bool,
        released: bool,
        exited: bool,
    }

    impl HostCleanupGate {
        fn wait_until_entered(&self) {
            self.wait_until(|state| state.entered, "slow invocation did not start");
        }

        fn wait_until_cancel_seen(&self) {
            self.wait_until(
                |state| state.cancel_seen,
                "slow invocation did not observe cancellation",
            );
        }

        fn release(&self) {
            let (state, changed) = &*self.state;
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            state.released = true;
            changed.notify_all();
        }

        fn exited(&self) -> bool {
            self.state
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .exited
        }

        fn wait_until(&self, predicate: impl Fn(&HostCleanupState) -> bool, message: &str) {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let (state, changed) = &*self.state;
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            while !predicate(&state) {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(!remaining.is_zero(), "{message}");
                let (next, timed_out) = changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|error| error.into_inner());
                state = next;
                assert!(!timed_out.timed_out(), "{message}");
            }
        }
    }

    struct CleanupGatedResourceTransport {
        gate: HostCleanupGate,
    }

    impl McpTransport for CleanupGatedResourceTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(json!({"capabilities": {"resources": {}}}))
        }

        fn list_tools(&self) -> Result<Value, String> {
            Ok(json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("cleanup resource transport does not support tools".to_string())
        }

        fn list_resources(&self) -> Result<Value, String> {
            Ok(json!({"resources": []}))
        }

        fn list_resources_or_cancel(
            &self,
            should_cancel: &dyn Fn() -> bool,
        ) -> Result<Value, String> {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let (state, changed) = &*self.gate.state;
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            state.entered = true;
            changed.notify_all();
            while !should_cancel() {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err("slow invocation was not cancelled".to_string());
                }
                let (next, _) = changed
                    .wait_timeout(state, remaining.min(Duration::from_millis(5)))
                    .unwrap_or_else(|error| error.into_inner());
                state = next;
            }
            state.cancel_seen = true;
            changed.notify_all();
            while !state.released {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err("slow invocation cleanup was not released".to_string());
                }
                let (next, _) = changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|error| error.into_inner());
                state = next;
            }
            state.exited = true;
            changed.notify_all();
            Err("MCP tool call cancelled".to_string())
        }

        fn list_resource_templates(&self) -> Result<Value, String> {
            Ok(json!({"resourceTemplates": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("cleanup resource transport does not support reads".to_string())
        }
    }

    #[derive(Clone, Default)]
    struct CompletionGate {
        state: Arc<(Mutex<bool>, Condvar)>,
    }

    impl CompletionGate {
        fn complete(&self) {
            let (state, changed) = &*self.state;
            *state.lock().unwrap_or_else(|error| error.into_inner()) = true;
            changed.notify_all();
        }

        fn wait(&self) {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let (state, changed) = &*self.state;
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            while !*state {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(!remaining.is_zero(), "fast invocation did not complete");
                let (next, timed_out) = changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|error| error.into_inner());
                state = next;
                assert!(!timed_out.timed_out(), "fast invocation did not complete");
            }
        }
    }

    struct ImmediateResourceTransport {
        completed: CompletionGate,
    }

    impl McpTransport for ImmediateResourceTransport {
        fn initialize(&self) -> Result<Value, String> {
            Ok(json!({"capabilities": {"resources": {}}}))
        }

        fn list_tools(&self) -> Result<Value, String> {
            Ok(json!({"tools": []}))
        }

        fn call_tool(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("immediate resource transport does not support tools".to_string())
        }

        fn list_resources(&self) -> Result<Value, String> {
            self.completed.complete();
            Ok(json!({
                "resources": [{"uri": "memo://fast", "name": "fast"}]
            }))
        }

        fn list_resource_templates(&self) -> Result<Value, String> {
            Ok(json!({"resourceTemplates": []}))
        }

        fn read_resource(&self, _uri: &str) -> Result<Value, String> {
            Err("immediate resource transport does not support reads".to_string())
        }
    }

    struct HostReadonlyExecutor {
        registry: McpRegistry,
        requests: Vec<ToolRequest>,
        results: Arc<Mutex<Vec<ToolResult>>>,
        calls: AtomicUsize,
    }

    impl ThreadOperationExecutor for HostReadonlyExecutor {
        fn run_turn(
            &self,
            thread: &mut RuntimeThread,
            _request: &HostedTurnRequest,
            generation: &GenerationContext,
            events: &mut EventFactory,
            writer: &mut (dyn io::Write + Send),
            cancel: &CancelToken,
        ) -> io::Result<ThreadOperationOutcome> {
            if self.calls.fetch_add(1, Ordering::AcqRel) > 0 {
                return Ok(RunStatus::Success.into());
            }
            let mut sink = EventSink::new(writer, generation.config().output_format);
            let execution = execute_readonly_batch(RuntimeReadonlyBatchContext {
                cwd: generation
                    .config()
                    .cwd
                    .as_deref()
                    .unwrap_or_else(|| Path::new(".")),
                events,
                sink: &mut sink,
                tool_requests: &self.requests,
                emit_deltas: true,
                mcp_registry: &self.registry,
                hooks: &HookRunner::default(),
                cancel,
                output_truncation: ToolOutputTruncation::default(),
                max_parallel: 2,
                provider_response_ingress: None,
            })?;
            let status = if execution
                .results
                .iter()
                .any(|result| result.status == ToolStatus::Cancelled)
            {
                RunStatus::Cancelled
            } else {
                RunStatus::Success
            };
            record_readonly_batch_results(
                thread.session_mut().conversation_mut(),
                None,
                execution.results.clone(),
                true,
            )?;
            *self
                .results
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = execution.results;
            if let Some(error) = execution.event_error {
                return Err(error);
            }
            Ok(status.into())
        }
    }

    #[derive(Clone, Default)]
    struct SharedJsonlWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for SharedJsonlWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SharedJsonlWriter {
        fn events(&self) -> Vec<EventEnvelope> {
            let bytes = self
                .bytes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            String::from_utf8(bytes)
                .expect("JSONL output is UTF-8")
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).expect("event JSONL"))
                .collect()
        }
    }

    fn host_test_config(cwd: PathBuf) -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(cwd),
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).expect("default model"),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
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

    #[test]
    fn readonly_batch_cancels_an_in_flight_mcp_resource_call() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let registry = McpRegistry::from_resource_transports_for_test([(
            "slow".to_string(),
            Box::new(BlockingResourceTransport {
                started: started_tx,
                release: Arc::clone(&release),
            }) as Box<dyn McpTransport>,
        )]);
        let request = ToolRequest {
            id: "list-slow-resources".to_string(),
            name: ToolName::ListMcpResources,
            action: ActionKind::Read,
            target: Some("slow".to_string()),
            raw_arguments: Some(json!({"server": "slow"}).to_string()),
        };
        assert!(should_run_readonly_batch(2, &request));

        let cancel = CancelToken::new();
        let worker_cancel = cancel.clone();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let mut events = EventFactory::new("readonly-cancel".to_string());
            let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
            let hooks = HookRunner::default();
            let execution = execute_readonly_batch(RuntimeReadonlyBatchContext {
                cwd: Path::new("."),
                events: &mut events,
                sink: &mut sink,
                tool_requests: &[request],
                emit_deltas: true,
                mcp_registry: &registry,
                hooks: &hooks,
                cancel: &worker_cancel,
                output_truncation: ToolOutputTruncation::default(),
                max_parallel: 1,
                provider_response_ingress: None,
            })
            .expect("execute read-only batch");
            let _ = done_tx.send(execution.results);
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("read-only MCP call should start");
        cancel.cancel();
        let prompt_result = done_rx.recv_timeout(Duration::from_millis(300));
        let completed_promptly = prompt_result.is_ok();

        if !completed_promptly {
            let (lock, wake) = &*release;
            *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
            wake.notify_all();
        }
        let results = match prompt_result {
            Ok(results) => results,
            Err(_) => done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("released legacy worker should finish"),
        };
        worker.join().expect("read-only batch worker");

        assert!(
            completed_promptly,
            "in-flight read-only MCP call did not observe turn cancellation"
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, ToolStatus::Cancelled);
        assert_eq!(results[0].terminal().started, ToolInvocationStarted::Yes);
    }

    #[test]
    fn runtime_host_interrupt_joins_readonly_invocations_before_next_turn() {
        let cleanup = HostCleanupGate::default();
        let fast = CompletionGate::default();
        let registry = McpRegistry::from_resource_transports_for_test([
            (
                "slow".to_string(),
                Box::new(CleanupGatedResourceTransport {
                    gate: cleanup.clone(),
                }) as Box<dyn McpTransport>,
            ),
            (
                "fast".to_string(),
                Box::new(ImmediateResourceTransport {
                    completed: fast.clone(),
                }) as Box<dyn McpTransport>,
            ),
        ]);
        let requests = vec![
            ToolRequest {
                id: "slow-call".to_string(),
                name: ToolName::ListMcpResources,
                action: ActionKind::Read,
                target: Some("slow".to_string()),
                raw_arguments: Some(json!({"server": "slow"}).to_string()),
            },
            ToolRequest {
                id: "fast-call".to_string(),
                name: ToolName::ListMcpResources,
                action: ActionKind::Read,
                target: Some("fast".to_string()),
                raw_arguments: Some(json!({"server": "fast"}).to_string()),
            },
        ];
        let results = Arc::new(Mutex::new(Vec::new()));
        let executor = Arc::new(HostReadonlyExecutor {
            registry,
            requests,
            results: Arc::clone(&results),
            calls: AtomicUsize::new(0),
        });
        let cwd = tempfile::tempdir().expect("temp cwd");
        let host = RuntimeHost::start_with_executor(executor).expect("start runtime host");
        let runtime_thread = host
            .start_thread(host_test_config(cwd.path().to_path_buf()), "read-only host")
            .expect("start runtime thread");
        let writer = SharedJsonlWriter::default();
        let first = runtime_thread
            .start_turn(HostedTurnRequest::new("first"), writer.clone())
            .expect("start first turn");
        cleanup.wait_until_entered();
        fast.wait();

        assert!(matches!(
            first.interrupt().expect("interrupt first turn"),
            InterruptOperationResult::Requested { .. }
        ));
        cleanup.wait_until_cancel_seen();
        assert!(
            first.wait_timeout(Duration::from_millis(50)).is_none(),
            "operation completed before invocation cleanup was joined"
        );
        assert!(matches!(
            runtime_thread.start_turn(HostedTurnRequest::new("too early"), io::sink()),
            Err(RuntimeHostError::OperationActive { .. })
        ));

        cleanup.release();
        assert_eq!(
            first
                .wait_timeout(Duration::from_secs(2))
                .expect("first terminal")
                .outcome(),
            &OperationOutcome::Completed(RunStatus::Cancelled)
        );
        assert!(cleanup.exited(), "slow invocation cleanup did not exit");

        let snapshot = runtime_thread.snapshot().expect("thread snapshot");
        let persisted_ids = snapshot
            .conversation()
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(persisted_ids, vec!["slow-call", "fast-call"]);

        let second = runtime_thread
            .start_turn(HostedTurnRequest::new("second"), writer.clone())
            .expect("start next turn after cleanup");
        assert_eq!(
            second
                .wait_timeout(Duration::from_secs(2))
                .expect("second terminal")
                .outcome(),
            &OperationOutcome::Completed(RunStatus::Success)
        );

        let results = results
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert_eq!(
            results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>(),
            vec!["slow-call", "fast-call"]
        );
        assert_eq!(results[0].status, ToolStatus::Cancelled);
        assert_eq!(results[1].status, ToolStatus::Completed);
        assert!(
            results
                .iter()
                .all(|result| { result.terminal().started == ToolInvocationStarted::Yes })
        );

        let completed_events = writer
            .events()
            .into_iter()
            .filter(|event| event.event_type == EventType::ToolCallCompleted)
            .collect::<Vec<_>>();
        assert_eq!(completed_events.len(), 2);
        assert_eq!(completed_events[0].payload["id"], "slow-call");
        assert_eq!(completed_events[1].payload["id"], "fast-call");

        host.shutdown().expect("shutdown runtime host");
    }
}
