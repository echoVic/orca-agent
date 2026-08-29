#![deny(deprecated)]

pub mod context;
pub mod deepseek_fixture;
pub mod deepseek_http;
pub mod http_client;
pub mod prompt_cache;
pub mod streaming;
pub mod summary_cache;
pub mod tool_schema;

use std::collections::VecDeque;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use orca_core::approval_types::ActionKind;
use orca_core::cancel::CancelToken;
use orca_core::config::{ProviderKind, ReasoningEffort};
use orca_core::conversation::{Conversation, Message, RawToolCall};
use orca_core::external_config::ExternalToolConfig;
use orca_core::provider_types::{
    ProviderError, ProviderErrorKind, ProviderResponse, ProviderStep, Usage,
};
use orca_core::tool_types::{ToolName, ToolRequest};
use orca_mcp::McpRegistry;

#[derive(Clone)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: ReasoningEffort,
    pub tools_override: Option<Vec<tool_schema::ProviderToolDefinition>>,
    pub mcp_registry: Option<McpRegistry>,
    pub external_tools: Vec<ExternalToolConfig>,
}

pub enum ProviderStreamEvent {
    Step(ProviderStreamDelivery),
    Completed(ProviderResponse),
}

pub struct ProviderStreamDelivery {
    step: ProviderStep,
    acknowledged: Option<mpsc::SyncSender<()>>,
}

impl ProviderStreamDelivery {
    pub fn step(&self) -> &ProviderStep {
        &self.step
    }
}

impl Drop for ProviderStreamDelivery {
    fn drop(&mut self) {
        if let Some(acknowledged) = self.acknowledged.take() {
            let _ = acknowledged.send(());
        }
    }
}

pub struct ProviderStreamingCall {
    receiver: Option<mpsc::Receiver<ProviderWorkerStep>>,
    worker: Option<JoinedProviderWorker>,
    replay_steps: VecDeque<ProviderStep>,
    completed: Option<ProviderResponse>,
}

impl ProviderStreamingCall {
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<ProviderStreamEvent, mpsc::RecvTimeoutError> {
        if let Some(step) = self.replay_steps.pop_front() {
            return Ok(ProviderStreamEvent::Step(ProviderStreamDelivery {
                step,
                acknowledged: None,
            }));
        }
        if let Some(response) = self.completed.take() {
            return Ok(ProviderStreamEvent::Completed(response));
        }

        let delivery = self
            .receiver
            .as_ref()
            .ok_or(mpsc::RecvTimeoutError::Disconnected)
            .and_then(|receiver| receiver.recv_timeout(timeout));
        match delivery {
            Ok(delivery) => Ok(ProviderStreamEvent::Step(ProviderStreamDelivery {
                step: delivery.step,
                acknowledged: Some(delivery.acknowledged),
            })),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(mpsc::RecvTimeoutError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.receiver.take();
                let worker = self
                    .worker
                    .take()
                    .ok_or(mpsc::RecvTimeoutError::Disconnected)?;
                match worker.join() {
                    Ok(response) => Ok(ProviderStreamEvent::Completed(response)),
                    Err(_) => {
                        let response =
                            provider_worker_error("provider worker panicked".to_string());
                        self.replay_steps.extend(response.steps.iter().cloned());
                        self.completed = Some(response);
                        self.recv_timeout(timeout)
                    }
                }
            }
        }
    }

    pub fn cancel(&self) {
        if let Some(worker) = self.worker.as_ref() {
            worker.cancel();
        }
    }

    pub fn cancel_and_join(&mut self) {
        self.cancel();
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ProviderStreamingCall {
    fn drop(&mut self) {
        self.cancel_and_join();
    }
}

pub fn call(
    kind: ProviderKind,
    conversation: &Conversation,
    config: &ProviderConfig,
) -> ProviderResponse {
    match kind {
        ProviderKind::Mock => mock_call(conversation),
        ProviderKind::DeepSeekFixture => {
            let has_tool_results = conversation
                .messages
                .iter()
                .any(|m| matches!(m, Message::Tool { .. }));

            if has_tool_results {
                let msg =
                    "DeepSeek fixture completed after reading repository context.".to_string();
                ProviderResponse {
                    steps: vec![ProviderStep::MessageDelta(msg.clone())],
                    assistant_content: Some(msg),
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                    usage: None,
                }
            } else {
                let steps = deepseek_fixture::plan();
                let tool_calls: Vec<RawToolCall> = steps
                    .iter()
                    .filter_map(|s| {
                        if let ProviderStep::ToolCall(req) = s {
                            Some(RawToolCall {
                                id: req.id.clone(),
                                function_name: req.name.as_str().to_string(),
                                arguments: req.raw_arguments.clone().unwrap_or_default(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                ProviderResponse {
                    steps,
                    assistant_content: None,
                    assistant_reasoning: Some(
                        "DeepSeek fixture reasoning: inspect the repository context before answering."
                            .to_string(),
                    ),
                    tool_calls,
                    usage: None,
                }
            }
        }
        ProviderKind::DeepSeek => deepseek_http::call(conversation, config),
    }
}

pub fn call_streaming(
    kind: ProviderKind,
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: &CancelToken,
    on_step: &mut dyn FnMut(&ProviderStep),
) -> ProviderResponse {
    const STREAM_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
    let mut stream = start_streaming(kind, conversation, config, cancel.clone());
    let callback_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            match stream.recv_timeout(STREAM_EVENT_POLL_INTERVAL) {
                Ok(ProviderStreamEvent::Step(delivery)) => on_step(delivery.step()),
                Ok(ProviderStreamEvent::Completed(response)) => return response,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let response = provider_worker_error(
                        "provider stream disconnected before completion".to_string(),
                    );
                    send_response_steps_to_callback(&response, on_step);
                    return response;
                }
            }
        }
    }));
    match callback_result {
        Ok(response) => response,
        Err(payload) => {
            stream.cancel_and_join();
            std::panic::resume_unwind(payload);
        }
    }
}

pub fn start_streaming(
    kind: ProviderKind,
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: CancelToken,
) -> ProviderStreamingCall {
    let conversation = conversation.clone();
    let config = config.clone();
    let worker_cancel = cancel.clone();
    let (step_tx, step_rx) = mpsc::sync_channel(0);
    let worker = match thread::Builder::new()
        .name("orca-provider-stream".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let response = provider_worker_error(format!(
                        "failed to start async provider runtime: {error}"
                    ));
                    send_response_steps(&step_tx, &worker_cancel, &response);
                    return response;
                }
            };
            let step_cancel = worker_cancel.clone();
            runtime.block_on(call_streaming_async(
                kind,
                &conversation,
                &config,
                &worker_cancel,
                move |step| {
                    let _ = send_worker_step(&step_tx, &step_cancel, step);
                },
            ))
        }) {
        Ok(worker) => worker,
        Err(error) => {
            let response =
                provider_worker_error(format!("failed to start provider worker: {error}"));
            return ProviderStreamingCall {
                receiver: None,
                worker: None,
                replay_steps: response.steps.iter().cloned().collect(),
                completed: Some(response),
            };
        }
    };
    ProviderStreamingCall {
        receiver: Some(step_rx),
        worker: Some(JoinedProviderWorker::new(worker, cancel)),
        replay_steps: VecDeque::new(),
        completed: None,
    }
}

pub async fn call_streaming_async(
    kind: ProviderKind,
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: &CancelToken,
    mut on_step: impl FnMut(&ProviderStep) + Send,
) -> ProviderResponse {
    match kind {
        ProviderKind::Mock => {
            if current_turn_has_tool_results(conversation) {
                let response = mock_call(conversation);
                for step in &response.steps {
                    on_step(step);
                }
                return response;
            }
            if let Some(key) = conversation
                .last_user_message()
                .and_then(|prompt| prompt.trim().strip_prefix("mock_stream_flaky_once "))
            {
                if mock_flaky_once_should_fail(key) {
                    let partial = ProviderStep::ReasoningDelta(
                        "Mock transient attempt emitted partial reasoning.".to_string(),
                    );
                    let error = ProviderStep::Error(ProviderError::new(
                        ProviderErrorKind::Transport,
                        format!("mock stream transport failure requested for {key}"),
                    ));
                    on_step(&partial);
                    on_step(&error);
                    return ProviderResponse {
                        steps: vec![partial, error],
                        assistant_content: None,
                        assistant_reasoning: None,
                        tool_calls: Vec::new(),
                        usage: None,
                    };
                }
                let message = format!("Mock runtime completed after stream recovery for {key}.");
                let completed = ProviderStep::MessageDelta(message.clone());
                on_step(&completed);
                return ProviderResponse {
                    steps: vec![completed],
                    assistant_content: Some(message),
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                    usage: None,
                };
            }
            if let Some((delay_ms, tool_prompt)) = mock_stream_tool_delay_ms(conversation) {
                let started =
                    ProviderStep::MessageDelta("Mock slow tool stream started.".to_string());
                on_step(&started);
                if !sleep_with_cancel(Duration::from_millis(delay_ms), cancel).await {
                    return ProviderResponse {
                        steps: vec![started],
                        assistant_content: Some("Mock slow tool stream started.".to_string()),
                        assistant_reasoning: None,
                        tool_calls: Vec::new(),
                        usage: None,
                    };
                }
                if let Some(tool_request) = parse_mock_prompt(&tool_prompt) {
                    let raw_call = RawToolCall {
                        id: tool_request.id.clone(),
                        function_name: tool_request.name.as_str().to_string(),
                        arguments: tool_request.raw_arguments.clone().unwrap_or_default(),
                    };
                    return ProviderResponse {
                        steps: vec![started, ProviderStep::ToolCall(tool_request)],
                        assistant_content: None,
                        assistant_reasoning: None,
                        tool_calls: vec![raw_call],
                        usage: None,
                    };
                }
            }
            if let Some(delay_ms) = mock_stream_delay_ms(conversation) {
                let started = ProviderStep::MessageDelta("Mock slow stream started.".to_string());
                on_step(&started);
                if !sleep_with_cancel(Duration::from_millis(delay_ms), cancel).await {
                    return ProviderResponse {
                        steps: vec![started],
                        assistant_content: Some("Mock slow stream started.".to_string()),
                        assistant_reasoning: None,
                        tool_calls: Vec::new(),
                        usage: None,
                    };
                }
                let completed =
                    ProviderStep::MessageDelta("Mock slow stream completed.".to_string());
                on_step(&completed);
                return ProviderResponse {
                    steps: vec![started, completed],
                    assistant_content: Some(
                        "Mock slow stream started.Mock slow stream completed.".to_string(),
                    ),
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                    usage: None,
                };
            }
            if let Some(release_marker) = mock_stream_release_marker(conversation) {
                let started =
                    ProviderStep::MessageDelta("Mock release-marker stream started.".to_string());
                on_step(&started);
                while !release_marker.exists() {
                    if !sleep_with_cancel(Duration::from_millis(10), cancel).await {
                        return ProviderResponse {
                            steps: vec![started],
                            assistant_content: Some(
                                "Mock release-marker stream started.".to_string(),
                            ),
                            assistant_reasoning: None,
                            tool_calls: Vec::new(),
                            usage: None,
                        };
                    }
                }
                let completed =
                    ProviderStep::MessageDelta("Mock release-marker stream completed.".to_string());
                on_step(&completed);
                return ProviderResponse {
                    steps: vec![started, completed],
                    assistant_content: Some(
                        "Mock release-marker stream started.Mock release-marker stream completed."
                            .to_string(),
                    ),
                    assistant_reasoning: None,
                    tool_calls: Vec::new(),
                    usage: None,
                };
            }
            if let Some(delay_ms) = mock_stream_usage_delay_ms(conversation) {
                let started =
                    ProviderStep::ReasoningDelta("Mock delayed usage started.".to_string());
                on_step(&started);
                if !sleep_with_cancel(Duration::from_millis(delay_ms), cancel).await {
                    return ProviderResponse {
                        steps: vec![started],
                        assistant_content: None,
                        assistant_reasoning: Some("Mock delayed usage started.".to_string()),
                        tool_calls: Vec::new(),
                        usage: None,
                    };
                }
                let completed =
                    ProviderStep::MessageDelta("Mock delayed usage completed.".to_string());
                on_step(&completed);
                return ProviderResponse {
                    steps: vec![started, completed],
                    assistant_content: Some("Mock delayed usage completed.".to_string()),
                    assistant_reasoning: Some("Mock delayed usage started.".to_string()),
                    tool_calls: Vec::new(),
                    usage: Some(Usage {
                        input_tokens: 120,
                        output_tokens: 30,
                        cache_tokens: 10,
                    }),
                };
            }
            let response = call(kind, conversation, config);
            for step in &response.steps {
                on_step(step);
            }
            if conversation
                .last_user_message()
                .is_some_and(|prompt| prompt.trim() == "mock_usage_then_cancel")
            {
                cancel.cancel();
            }
            response
        }
        ProviderKind::DeepSeekFixture => {
            let response = call(kind, conversation, config);
            for step in &response.steps {
                on_step(step);
            }
            response
        }
        ProviderKind::DeepSeek => {
            deepseek_http::call_streaming_async(conversation, config, cancel, on_step).await
        }
    }
}

async fn sleep_with_cancel(delay: Duration, cancel: &CancelToken) -> bool {
    tokio::select! {
        biased;
        _ = http_client::wait_for_cancel(cancel) => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

fn provider_worker_error(message: String) -> ProviderResponse {
    ProviderResponse {
        steps: vec![ProviderStep::Error(ProviderError::new(
            ProviderErrorKind::Transport,
            format!("provider worker error: {message}"),
        ))],
        assistant_content: None,
        assistant_reasoning: None,
        tool_calls: Vec::new(),
        usage: None,
    }
}

struct ProviderWorkerStep {
    step: ProviderStep,
    acknowledged: mpsc::SyncSender<()>,
}

fn send_worker_step(
    sender: &mpsc::SyncSender<ProviderWorkerStep>,
    cancel: &CancelToken,
    step: &ProviderStep,
) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    let (acknowledged, acknowledgement) = mpsc::sync_channel(0);
    if sender
        .send(ProviderWorkerStep {
            step: step.clone(),
            acknowledged,
        })
        .is_err()
    {
        cancel.cancel();
        return false;
    }
    if acknowledgement.recv().is_err() {
        cancel.cancel();
        return false;
    }
    !cancel.is_cancelled()
}

fn send_response_steps(
    sender: &mpsc::SyncSender<ProviderWorkerStep>,
    cancel: &CancelToken,
    response: &ProviderResponse,
) {
    for step in &response.steps {
        if !send_worker_step(sender, cancel, step) {
            break;
        }
    }
}

fn send_response_steps_to_callback(
    response: &ProviderResponse,
    on_step: &mut dyn FnMut(&ProviderStep),
) {
    for step in &response.steps {
        on_step(step);
    }
}

struct JoinedProviderWorker {
    handle: Option<thread::JoinHandle<ProviderResponse>>,
    cancel: CancelToken,
}

impl JoinedProviderWorker {
    fn new(handle: thread::JoinHandle<ProviderResponse>, cancel: CancelToken) -> Self {
        Self {
            handle: Some(handle),
            cancel,
        }
    }

    fn join(mut self) -> thread::Result<ProviderResponse> {
        self.handle.take().expect("provider worker handle").join()
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for JoinedProviderWorker {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.cancel.cancel();
            let _ = handle.join();
        }
    }
}

fn mock_stream_tool_delay_ms(conversation: &Conversation) -> Option<(u64, String)> {
    let rest = conversation
        .last_user_message()
        .unwrap_or("")
        .trim()
        .strip_prefix("mock_stream_tool_delay_ms ")?;
    let (delay, tool_prompt) = rest.split_once(' ')?;
    let delay = delay.trim().parse::<u64>().ok()?.min(10_000);
    Some((delay, tool_prompt.trim().to_string()))
}

fn mock_stream_delay_ms(conversation: &Conversation) -> Option<u64> {
    conversation
        .last_user_message()
        .unwrap_or("")
        .trim()
        .strip_prefix("mock_stream_delay_ms ")
        .and_then(|delay| delay.trim().parse::<u64>().ok())
        .map(|delay| delay.min(10_000))
}

fn mock_stream_release_marker(conversation: &Conversation) -> Option<std::path::PathBuf> {
    let marker = conversation
        .last_user_message()
        .unwrap_or("")
        .trim()
        .strip_prefix("mock_stream_release_marker ")?
        .trim();
    (!marker.is_empty()).then(|| std::path::PathBuf::from(marker))
}

fn mock_stream_usage_delay_ms(conversation: &Conversation) -> Option<u64> {
    conversation
        .last_user_message()
        .unwrap_or("")
        .trim()
        .strip_prefix("mock_stream_usage_delay_ms ")
        .and_then(|delay| delay.trim().parse::<u64>().ok())
        .map(|delay| delay.min(10_000))
}

fn current_turn_has_tool_results(conversation: &Conversation) -> bool {
    conversation
        .messages
        .iter()
        .rev()
        .take_while(|message| !matches!(message, Message::User { .. }))
        .any(|message| matches!(message, Message::Tool { .. }))
}

fn mock_repeat_read_state(conversation: &Conversation) -> Option<(usize, usize, usize)> {
    let latest_user_index = conversation
        .messages
        .iter()
        .rposition(|message| matches!(message, Message::User { .. }))?;
    let prompt = match &conversation.messages[latest_user_index] {
        Message::User { content, .. } => content,
        _ => unreachable!("latest user index must point to a user message"),
    };
    let requested_tools = prompt
        .trim()
        .strip_prefix("mock_repeat_read ")?
        .trim()
        .parse::<usize>()
        .ok()?
        .clamp(1, 256);
    let request_number = conversation
        .messages
        .iter()
        .take(latest_user_index + 1)
        .filter(|message| {
            matches!(
                message,
                Message::User { content, .. }
                    if content.trim().starts_with("mock_repeat_read ")
            )
        })
        .count();
    let completed_tools = conversation
        .messages
        .iter()
        .skip(latest_user_index + 1)
        .filter(|message| {
            matches!(
                message,
                Message::Tool { tool_call_id, .. }
                    if tool_call_id.starts_with("mock-repeat-read-")
            )
        })
        .count();
    Some((requested_tools, request_number, completed_tools))
}

fn mock_call(conversation: &Conversation) -> ProviderResponse {
    let has_tool_results = current_turn_has_tool_results(conversation);
    let prompt = conversation.last_user_message().unwrap_or("");

    if conversation.messages.iter().any(|message| {
        matches!(
            message,
            Message::System { content, .. } if content.starts_with("AUTO_MEMORY_EXTRACTOR_V2")
        )
    }) {
        let msg = if prompt.contains("mock_auto_memory_malformed") {
            "malformed automatic memory response".to_string()
        } else {
            "- project: Release qualification requires a clean install smoke test.".to_string()
        };
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(msg.clone())],
            assistant_content: Some(msg),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if let Some((requested_tools, request_number, completed_tools)) =
        mock_repeat_read_state(conversation)
        && completed_tools < requested_tools
    {
        let tool_request = ToolRequest {
            id: format!("mock-repeat-read-{request_number}-{}", completed_tools + 1),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("README.md".to_string()),
            raw_arguments: Some(serde_json::json!({ "path": "README.md" }).to_string()),
        };
        let raw_call = RawToolCall {
            id: tool_request.id.clone(),
            function_name: tool_request.name.as_str().to_string(),
            arguments: tool_request.raw_arguments.clone().unwrap_or_default(),
        };
        return ProviderResponse {
            steps: vec![ProviderStep::ToolCall(tool_request)],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: vec![raw_call],
            usage: None,
        };
    }

    if prompt.trim() == "mock_provider_error" {
        return ProviderResponse {
            steps: vec![ProviderStep::Error(ProviderError::other(
                "mock provider error: api_key=super-secret",
            ))],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if prompt.trim() == "bad_plan_then_fix" && has_tool_results {
        let has_fixed_plan = conversation.messages.iter().any(|m| match m {
            Message::Tool { content, .. } => content.contains("Plan updated"),
            _ => false,
        });
        if has_fixed_plan {
            let msg = "Mock completed after fixing malformed tool arguments.".to_string();
            return ProviderResponse {
                steps: vec![ProviderStep::MessageDelta(msg.clone())],
                assistant_content: Some(msg),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            };
        }
        let saw_schema_error = conversation.messages.iter().any(|m| match m {
            Message::Tool { content, .. } => {
                content.contains("tool arguments failed schema validation")
            }
            _ => false,
        });
        if saw_schema_error {
            let mut tool_request =
                valid_mock_plan_request(Some("Recovered from schema validation failure"));
            tool_request.id = "mock-tool-2".to_string();
            let raw_call = RawToolCall {
                id: tool_request.id.clone(),
                function_name: tool_request.name.as_str().to_string(),
                arguments: tool_request.raw_arguments.clone().unwrap_or_default(),
            };
            return ProviderResponse {
                steps: vec![ProviderStep::ToolCall(tool_request)],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: vec![raw_call],
                usage: None,
            };
        }
    }

    if prompt.trim() == "unknown_tool_then_fix" {
        let saw_unknown_tool_error = conversation.messages.iter().any(|message| match message {
            Message::Tool { content, .. } => content.contains("unknown tool: wc -l"),
            _ => false,
        });
        if saw_unknown_tool_error {
            let message = "Mock completed after correcting unknown tool call.".to_string();
            return ProviderResponse {
                steps: vec![ProviderStep::MessageDelta(message.clone())],
                assistant_content: Some(message),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            };
        }

        if !has_tool_results {
            let tool_request = ToolRequest {
                id: "mock-unknown-tool-1".to_string(),
                name: ToolName::External("wc -l".to_string()),
                action: ActionKind::Read,
                target: Some("wc -l".to_string()),
                raw_arguments: Some("{}".to_string()),
            };
            let raw_call = RawToolCall {
                id: tool_request.id.clone(),
                function_name: tool_request.name.as_str().to_string(),
                arguments: tool_request.raw_arguments.clone().unwrap_or_default(),
            };
            return ProviderResponse {
                steps: vec![ProviderStep::ToolCall(tool_request)],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: vec![raw_call],
                usage: None,
            };
        }
    }

    if (prompt.trim().starts_with("workflow_read_messages ")
        || prompt.trim().starts_with("workflow_list_tasks "))
        && has_tool_results
    {
        let tool_outputs = conversation
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let msg = format!("Workflow IPC result: {tool_outputs}");
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(msg.clone())],
            assistant_content: Some(msg),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if prompt == "workflow draft action save" && has_tool_results {
        let action_completed = conversation.messages.iter().any(|message| match message {
            Message::Tool { content, .. } => serde_json::from_str::<serde_json::Value>(content)
                .ok()
                .is_some_and(|value| {
                    value.get("status").and_then(serde_json::Value::as_str) == Some("saved")
                        && value.get("action").and_then(serde_json::Value::as_str) == Some("save")
                }),
            _ => false,
        });
        if action_completed {
            let msg = "Mock saved workflow draft.".to_string();
            return ProviderResponse {
                steps: vec![ProviderStep::MessageDelta(msg.clone())],
                assistant_content: Some(msg),
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            };
        }

        let draft_id = conversation
            .messages
            .iter()
            .find_map(|message| match message {
                Message::Tool { content, .. } => serde_json::from_str::<serde_json::Value>(content)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("draftId")
                            .and_then(serde_json::Value::as_str)
                            .map(ToString::to_string)
                    }),
                _ => None,
            });
        if let Some(draft_id) = draft_id {
            let tool_request = ToolRequest {
                id: "mock-tool-2".to_string(),
                name: ToolName::WorkflowDraftAction,
                action: ActionKind::Write,
                target: Some(draft_id.clone()),
                raw_arguments: Some(
                    serde_json::json!({
                        "draftId": draft_id,
                        "action": "save",
                        "scope": "project"
                    })
                    .to_string(),
                ),
            };
            let raw_call = RawToolCall {
                id: tool_request.id.clone(),
                function_name: tool_request.name.as_str().to_string(),
                arguments: tool_request.raw_arguments.clone().unwrap_or_default(),
            };
            return ProviderResponse {
                steps: vec![ProviderStep::ToolCall(tool_request)],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: vec![raw_call],
                usage: None,
            };
        }
    }

    if prompt == "mcp__broken__tool" && has_tool_results {
        return ProviderResponse {
            steps: vec![ProviderStep::Error(ProviderError::other(
                "mcp__broken__tool failed in mock provider",
            ))],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if !has_tool_results && let Some(command) = prompt.trim().strip_prefix("force_bash ") {
        let tool_id = (1_u64..)
            .map(|index| format!("mock-tool-{index}"))
            .find(|candidate| {
                !conversation.messages.iter().any(|message| match message {
                    Message::Assistant { tool_calls, .. } => tool_calls
                        .iter()
                        .any(|tool_call| tool_call.id == *candidate),
                    Message::Tool { tool_call_id, .. } => tool_call_id == candidate,
                    Message::System { .. } | Message::User { .. } => false,
                })
            })
            .expect("mock tool id space is not exhausted");
        let bash = ToolRequest {
            id: tool_id,
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(command.to_string()),
            raw_arguments: Some(serde_json::json!({ "command": command }).to_string()),
        };
        let raw_call = RawToolCall {
            id: bash.id.clone(),
            function_name: bash.name.as_str().to_string(),
            arguments: bash.raw_arguments.clone().unwrap_or_default(),
        };
        return ProviderResponse {
            steps: vec![ProviderStep::ToolCall(bash)],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: vec![raw_call],
            usage: None,
        };
    }

    if matches!(
        prompt,
        "task_stop_main_session" | "task_stop_main_session_with_siblings"
    ) && let Some(task_id) = find_mock_main_session_task_id(conversation)
    {
        let task_stop = ToolRequest {
            id: "mock-tool-2".to_string(),
            name: ToolName::TaskStop,
            action: ActionKind::Write,
            target: Some(task_id.clone()),
            raw_arguments: Some(serde_json::json!({ "task_id": task_id }).to_string()),
        };
        let mut requests = vec![task_stop];
        if prompt == "task_stop_main_session_with_siblings" {
            requests.extend(
                ["first", "second"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| ToolRequest {
                        id: format!("mock-tool-{}", index + 3),
                        name: ToolName::Bash,
                        action: ActionKind::Shell,
                        target: Some(format!("printf {label}")),
                        raw_arguments: Some(
                            serde_json::json!({ "command": format!("printf {label}") }).to_string(),
                        ),
                    }),
            );
        }
        let tool_calls = requests
            .iter()
            .map(|request| RawToolCall {
                id: request.id.clone(),
                function_name: request.name.as_str().to_string(),
                arguments: request.raw_arguments.clone().unwrap_or_default(),
            })
            .collect();
        return ProviderResponse {
            steps: requests.into_iter().map(ProviderStep::ToolCall).collect(),
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls,
            usage: None,
        };
    }

    if has_tool_results {
        let msg = "Mock completed after tool execution.".to_string();
        return ProviderResponse {
            steps: vec![
                ProviderStep::ReasoningDelta("Mock reasoning.".to_string()),
                ProviderStep::MessageDelta(msg.clone()),
            ],
            assistant_content: Some(msg),
            assistant_reasoning: Some("Mock reasoning.".to_string()),
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if prompt.trim() == "mock_fail" {
        return ProviderResponse {
            steps: vec![ProviderStep::Error(ProviderError::other(
                "mock child failure requested",
            ))],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if let Some(key) = prompt.trim().strip_prefix("mock_flaky_once ") {
        if mock_flaky_once_should_fail(key) {
            return ProviderResponse {
                steps: vec![ProviderStep::Error(ProviderError::new(
                    ProviderErrorKind::Transport,
                    format!("mock transient failure requested for {key}"),
                ))],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage: None,
            };
        }
        let message = format!("Mock runtime completed after transient failure for {key}.");
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(message.clone())],
            assistant_content: Some(message),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if matches!(prompt.trim(), "mock_usage" | "mock_usage_then_cancel") {
        let reasoning = "Mock runtime is preserving the DeepSeek reasoning channel.";
        let message = "Mock runtime completed with usage accounting.";
        return ProviderResponse {
            steps: vec![
                ProviderStep::ReasoningDelta(reasoning.to_string()),
                ProviderStep::MessageDelta(message.to_string()),
            ],
            assistant_content: Some(message.to_string()),
            assistant_reasoning: Some(reasoning.to_string()),
            tool_calls: Vec::new(),
            usage: Some(Usage {
                input_tokens: 120,
                output_tokens: 30,
                cache_tokens: 10,
            }),
        };
    }

    if prompt.trim() == "mock_proposed_plan" {
        let message = "Preface\n<proposed_plan>\n# Final plan\n- first\n- second\n</proposed_plan>\nPostscript";
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(message.to_string())],
            assistant_content: Some(message.to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if prompt
        .lines()
        .any(|line| line.trim() == "mock_history_echo")
    {
        let users = conversation
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ");
        let message = format!("Mock history users: {users}");
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(message.clone())],
            assistant_content: Some(message),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if prompt.trim() == "mock_system_echo" {
        let systems = conversation
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::System { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ");
        let message = format!("Mock system messages: {systems}");
        return ProviderResponse {
            steps: vec![ProviderStep::MessageDelta(message.clone())],
            assistant_content: Some(message),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if prompt.trim() == "mock_silent_final" {
        return ProviderResponse {
            steps: Vec::new(),
            assistant_content: Some("Mock silent final response.".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
    }

    if prompt.trim() == "subagent batch schema_fail" {
        let mut first = parse_mock_prompt("subagent schema_ok").expect("schema ok request");
        first.id = "mock-tool-1".to_string();
        let mut second = parse_mock_prompt("subagent schema_fail").expect("schema fail request");
        second.id = "mock-tool-2".to_string();
        let steps = vec![
            ProviderStep::ToolCall(first.clone()),
            ProviderStep::ToolCall(second.clone()),
        ];
        let tool_calls = vec![
            RawToolCall {
                id: first.id,
                function_name: first.name.as_str().to_string(),
                arguments: first.raw_arguments.unwrap_or_default(),
            },
            RawToolCall {
                id: second.id,
                function_name: second.name.as_str().to_string(),
                arguments: second.raw_arguments.unwrap_or_default(),
            },
        ];
        return ProviderResponse {
            steps,
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls,
            usage: None,
        };
    }

    if prompt.trim() == "subagent batch terminal_boundary" {
        let mut first = parse_mock_prompt("subagent schema_fail").expect("schema fail request");
        first.id = "mock-tool-1".to_string();
        let mut second = parse_mock_prompt("subagent schema_ok").expect("schema ok request");
        second.id = "mock-tool-2".to_string();
        let mut third = parse_mock_prompt("subagent schema_ok").expect("schema ok request");
        third.id = "mock-tool-3".to_string();
        let requests = vec![first, second, third];
        let tool_calls = requests
            .iter()
            .map(|request| RawToolCall {
                id: request.id.clone(),
                function_name: request.name.as_str().to_string(),
                arguments: request.raw_arguments.clone().unwrap_or_default(),
            })
            .collect();
        return ProviderResponse {
            steps: requests.into_iter().map(ProviderStep::ToolCall).collect(),
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls,
            usage: None,
        };
    }

    if let Some(rest) = prompt.trim().strip_prefix("request_permissions_then_bash ")
        && let Some((root, command)) = rest.split_once(" :: ")
    {
        let request_permissions = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::RequestPermissions,
            action: ActionKind::Write,
            target: Some(root.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "reason": "mock provider needs a temporary write root",
                    "permissions": {
                        "fileSystem": {
                            "read": null,
                            "write": [root]
                        },
                        "network": null
                    }
                })
                .to_string(),
            ),
        };
        let bash = ToolRequest {
            id: "mock-tool-2".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(command.to_string()),
            raw_arguments: Some(serde_json::json!({ "command": command }).to_string()),
        };
        let tool_calls = vec![
            RawToolCall {
                id: request_permissions.id.clone(),
                function_name: request_permissions.name.as_str().to_string(),
                arguments: request_permissions
                    .raw_arguments
                    .clone()
                    .unwrap_or_default(),
            },
            RawToolCall {
                id: bash.id.clone(),
                function_name: bash.name.as_str().to_string(),
                arguments: bash.raw_arguments.clone().unwrap_or_default(),
            },
        ];
        return ProviderResponse {
            steps: vec![
                ProviderStep::ToolCall(request_permissions),
                ProviderStep::ToolCall(bash),
            ],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls,
            usage: None,
        };
    }

    if let Some(domain) = prompt
        .trim()
        .strip_prefix("request_network_permissions_then_done ")
        .map(str::trim)
        .filter(|domain| !domain.is_empty())
    {
        let request_permissions = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::RequestPermissions,
            action: ActionKind::Network,
            target: Some(domain.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "reason": "mock provider needs temporary network access",
                    "permissions": {
                        "fileSystem": null,
                        "network": {
                            "enabled": true,
                            "domains": {
                                domain: "allow"
                            }
                        }
                    }
                })
                .to_string(),
            ),
        };
        return ProviderResponse {
            steps: vec![
                ProviderStep::ToolCall(request_permissions.clone()),
                ProviderStep::MessageDelta("done".to_string()),
            ],
            assistant_content: Some("done".to_string()),
            assistant_reasoning: None,
            tool_calls: vec![RawToolCall {
                id: request_permissions.id.clone(),
                function_name: request_permissions.name.as_str().to_string(),
                arguments: request_permissions.raw_arguments.unwrap_or_default(),
            }],
            usage: None,
        };
    }

    if let Some(tool_request) = parse_mock_prompt(prompt) {
        let raw_call = RawToolCall {
            id: tool_request.id.clone(),
            function_name: tool_request.name.as_str().to_string(),
            arguments: tool_request.raw_arguments.clone().unwrap_or_default(),
        };
        let reasoning = "Mock runtime is preserving the DeepSeek reasoning channel.".to_string();
        ProviderResponse {
            steps: vec![
                ProviderStep::ReasoningDelta(reasoning.clone()),
                ProviderStep::ToolCall(tool_request),
            ],
            assistant_content: None,
            assistant_reasoning: Some(reasoning),
            tool_calls: vec![raw_call],
            usage: None,
        }
    } else {
        let reasoning = "Mock runtime is preserving the DeepSeek reasoning channel.";
        let message = "Mock runtime completed the headless harness contract.";
        ProviderResponse {
            steps: vec![
                ProviderStep::ReasoningDelta(reasoning.to_string()),
                ProviderStep::MessageDelta(message.to_string()),
            ],
            assistant_content: Some(message.to_string()),
            assistant_reasoning: Some(reasoning.to_string()),
            tool_calls: Vec::new(),
            usage: None,
        }
    }
}

fn mock_flaky_once_should_fail(key: &str) -> bool {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
        .lock()
        .map(|mut keys| keys.insert(key.to_string()))
        .unwrap_or(false)
}

fn parse_mock_prompt(prompt: &str) -> Option<ToolRequest> {
    let prompt = prompt.trim();

    if let Some(rest) = prompt.strip_prefix("read ") {
        let path = rest.to_string();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some(path.clone()),
            raw_arguments: Some(serde_json::json!({ "path": path }).to_string()),
        });
    }

    if prompt == "git status" {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::GitStatus,
            action: ActionKind::Read,
            target: Some(".".to_string()),
            raw_arguments: None,
        });
    }

    if let Some(rest) = prompt.strip_prefix("subagent ") {
        let (isolation, rest) = if let Some(description) = rest.trim().strip_prefix("worktree ") {
            (Some("worktree"), description.trim())
        } else {
            (None, rest.trim())
        };
        let (mode, description) = if let Some(description) = rest.strip_prefix("async ") {
            (Some("async"), description.trim())
        } else {
            (None, rest)
        };
        let prompt = if description == "mock_fail" {
            "mock_fail".to_string()
        } else if description == "schema_ok" || description == "schema_fail" {
            "mock_usage".to_string()
        } else {
            description.to_string()
        };
        let mut arguments = serde_json::json!({
            "description": description,
            "prompt": prompt
        });
        if description == "schema_ok" {
            arguments["schema"] = serde_json::json!({ "type": "string" });
        } else if description == "schema_fail" {
            arguments["schema"] = serde_json::json!({
                "type": "object",
                "required": ["result"],
                "properties": {
                    "result": { "type": "string" }
                }
            });
        }
        if let Some(mode) = mode {
            arguments["mode"] = serde_json::Value::String(mode.to_string());
        }
        if let Some(isolation) = isolation {
            arguments["isolation"] = serde_json::Value::String(isolation.to_string());
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some(description.to_string()),
            raw_arguments: Some(arguments.to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("subagent_status ") {
        let agent_id = rest.trim();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::SubagentStatus,
            action: ActionKind::Read,
            target: Some(agent_id.to_string()),
            raw_arguments: Some(serde_json::json!({ "agent_id": agent_id }).to_string()),
        });
    }

    if prompt == "task_list" {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({}).to_string()),
        });
    }

    if matches!(
        prompt,
        "task_stop_main_session" | "task_stop_main_session_with_siblings"
    ) {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({}).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("task_stop ") {
        let task_id = rest.trim();
        if task_id.is_empty() {
            return None;
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskStop,
            action: ActionKind::Write,
            target: Some(task_id.to_string()),
            raw_arguments: Some(serde_json::json!({ "task_id": task_id }).to_string()),
        });
    }

    if prompt == "mcp__broken__tool" {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Mcp("mcp__broken__tool".to_string()),
            action: ActionKind::Agent,
            target: Some("mcp__broken__tool".to_string()),
            raw_arguments: Some(serde_json::json!({}).to_string()),
        });
    }

    if prompt == "mcp__slow__wait" {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Mcp("mcp__slow__wait".to_string()),
            action: ActionKind::Agent,
            target: Some("mcp__slow__wait".to_string()),
            raw_arguments: Some(serde_json::json!({}).to_string()),
        });
    }

    if prompt == "workflow draft" {
        let script = "export const meta = { name: 'mock-workflow', description: 'Mock workflow', phases: ['main'] };\nconst result = await phase('main', async () => agent('inspect repo'));\nexport default result;";
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::WorkflowDraft,
            action: ActionKind::Write,
            target: Some("mock-workflow".to_string()),
            raw_arguments: Some(serde_json::json!({ "script": script }).to_string()),
        });
    }

    if prompt == "workflow draft action save" {
        let script = "export const meta = { name: 'mock-workflow', description: 'Mock workflow', phases: ['main'] };\nconst result = await phase('main', async () => agent('inspect repo'));\nexport default result;";
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::WorkflowDraft,
            action: ActionKind::Write,
            target: Some("mock-workflow".to_string()),
            raw_arguments: Some(serde_json::json!({ "script": script }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("workflow ") {
        let mode = rest.trim();
        let script = if mode == "inline" {
            "export const meta = { name: 'mock-workflow', description: 'Mock workflow', phases: ['main'] };\nconst until = Date.now() + 900;\nwhile (Date.now() < until) {}\nconst result = await phase('main', async () => agent('inspect repo'));\nexport default result;"
        } else {
            "export const meta = { name: 'mock-workflow', description: 'Mock workflow', phases: ['main'] };\nconst result = await phase('main', async () => agent('inspect repo'));\nexport default result;"
        };
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Workflow,
            action: ActionKind::Agent,
            target: Some(mode.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "script": script,
                    "args": { "mode": mode }
                })
                .to_string(),
            ),
        });
    }

    if let Some(rest) = prompt.strip_prefix("workflow_send_message ") {
        let mut parts = rest.splitn(3, ' ');
        let channel = parts.next()?.trim();
        let from = parts.next()?.trim();
        let message = parts.next()?.trim();
        if channel.is_empty() || from.is_empty() || message.is_empty() {
            return None;
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::WorkflowSendMessage,
            action: ActionKind::Agent,
            target: Some(channel.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "channel": channel,
                    "from": from,
                    "message": message
                })
                .to_string(),
            ),
        });
    }

    if let Some(rest) = prompt.strip_prefix("workflow_read_messages ") {
        let channel = rest.trim();
        if channel.is_empty() {
            return None;
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::WorkflowReadMessages,
            action: ActionKind::Agent,
            target: Some(channel.to_string()),
            raw_arguments: Some(serde_json::json!({ "channel": channel }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("workflow_clear_messages ") {
        let channel = rest.trim();
        if channel.is_empty() {
            return None;
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::WorkflowClearMessages,
            action: ActionKind::Agent,
            target: Some(channel.to_string()),
            raw_arguments: Some(serde_json::json!({ "channel": channel }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("workflow_create_task_list ") {
        let mut parts = rest.split_whitespace();
        let name = parts.next()?.trim();
        let items = parts
            .map(|item| serde_json::Value::String(item.to_string()))
            .collect::<Vec<_>>();
        if name.is_empty() {
            return None;
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::WorkflowCreateTaskList,
            action: ActionKind::Agent,
            target: Some(name.to_string()),
            raw_arguments: Some(serde_json::json!({ "name": name, "items": items }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("workflow_claim_task ") {
        let mut parts = rest.splitn(2, ' ');
        let name = parts.next()?.trim();
        let by = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            return None;
        }
        let mut arguments = serde_json::json!({ "name": name });
        if !by.is_empty() {
            arguments["by"] = serde_json::Value::String(by.to_string());
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::WorkflowClaimTask,
            action: ActionKind::Agent,
            target: Some(name.to_string()),
            raw_arguments: Some(arguments.to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("workflow_complete_task ") {
        let mut parts = rest.splitn(4, ' ');
        let name = parts.next()?.trim();
        let task_id = parts.next()?.trim();
        let by = parts.next()?.trim();
        let result = parts.next().unwrap_or("").trim();
        if name.is_empty() || task_id.is_empty() || by.is_empty() {
            return None;
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::WorkflowCompleteTask,
            action: ActionKind::Agent,
            target: Some(task_id.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "name": name,
                    "task_id": task_id,
                    "by": by,
                    "result": result
                })
                .to_string(),
            ),
        });
    }

    if let Some(rest) = prompt.strip_prefix("workflow_list_tasks ") {
        let name = rest.trim();
        if name.is_empty() {
            return None;
        }
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::WorkflowListTasks,
            action: ActionKind::Agent,
            target: Some(name.to_string()),
            raw_arguments: Some(serde_json::json!({ "name": name }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("plan ") {
        let explanation = if rest.trim().is_empty() {
            None
        } else {
            Some(rest.trim())
        };
        return Some(valid_mock_plan_request(explanation));
    }

    if prompt == "bad_plan_then_fix" {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::UpdatePlan,
            action: ActionKind::Read,
            target: Some("1 items".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "plan": [
                        {
                            "completed": "Inspect references"
                        }
                    ]
                })
                .to_string(),
            ),
        });
    }

    if let Some(rest) = prompt.strip_prefix("ask ") {
        let question = rest.trim();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::AskUserQuestion,
            action: ActionKind::Read,
            target: Some(question.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "questions": [{
                        "header": "Confirm",
                        "question": question,
                        "options": [
                            {"label": "yes", "description": "Continue"},
                            {"label": "no", "description": "Stop"}
                        ]
                    }]
                })
                .to_string(),
            ),
        });
    }

    if let Some(rest) = prompt.strip_prefix("grep ") {
        let pattern = rest.to_string();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Grep,
            action: ActionKind::Read,
            target: Some(pattern.clone()),
            raw_arguments: Some(serde_json::json!({ "pattern": pattern }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("bash ") {
        let command = rest.to_string();
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(command.clone()),
            raw_arguments: Some(serde_json::json!({ "command": command }).to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("external ")
        && let Some((name, args)) = rest.trim().split_once(' ')
    {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::External(name.to_string()),
            action: ActionKind::Write,
            target: Some(name.to_string()),
            raw_arguments: Some(args.to_string()),
        });
    }

    if let Some(rest) = prompt.strip_prefix("edit ")
        && let Some((file, replacement)) = rest.split_once(" :: ")
    {
        let (old, new) = replacement.split_once(" => ").unwrap_or((replacement, ""));
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some(file.to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "path": file,
                    "old_text": old,
                    "new_text": new
                })
                .to_string(),
            ),
        });
    }

    if prompt.contains("write") {
        return Some(ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some("file.txt".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "path": "file.txt",
                    "old_text": "placeholder",
                    "new_text": "content"
                })
                .to_string(),
            ),
        });
    }

    None
}

fn find_mock_main_session_task_id(conversation: &Conversation) -> Option<String> {
    conversation.messages.iter().find_map(|message| {
        let Message::Tool { content, .. } = message else {
            return None;
        };
        let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
        value
            .get("tasks")?
            .as_array()?
            .iter()
            .find(|task| {
                task.get("task_type").and_then(serde_json::Value::as_str) == Some("main_session")
                    && task.get("status").and_then(serde_json::Value::as_str) == Some("running")
            })?
            .get("id")?
            .as_str()
            .map(ToString::to_string)
    })
}

fn valid_mock_plan_request(explanation: Option<&str>) -> ToolRequest {
    let arguments = serde_json::json!({
        "explanation": explanation,
        "plan": [
            {
                "step": "Inspect references",
                "status": "completed"
            },
            {
                "step": "Implement task plan support",
                "status": "in_progress"
            },
            {
                "step": "Verify behavior",
                "status": "pending"
            }
        ]
    })
    .to_string();
    ToolRequest {
        id: "mock-tool-1".to_string(),
        name: ToolName::UpdatePlan,
        action: ActionKind::Read,
        target: Some("3 items".to_string()),
        raw_arguments: Some(arguments),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_prompt_parses_async_subagent_mode() {
        let request = parse_mock_prompt("subagent async inspect repo").expect("tool request");
        assert_eq!(request.name, ToolName::Subagent);
        assert_eq!(request.target.as_deref(), Some("inspect repo"));

        let arguments: serde_json::Value =
            serde_json::from_str(request.raw_arguments.as_deref().unwrap()).unwrap();
        assert_eq!(arguments["description"], "inspect repo");
        assert_eq!(arguments["prompt"], "inspect repo");
        assert_eq!(arguments["mode"], "async");
    }

    #[test]
    fn mock_prompt_parses_subagent_schema() {
        let request = parse_mock_prompt("subagent schema_fail").expect("tool request");
        assert_eq!(request.name, ToolName::Subagent);

        let arguments: serde_json::Value =
            serde_json::from_str(request.raw_arguments.as_deref().unwrap()).unwrap();
        assert_eq!(arguments["description"], "schema_fail");
        assert_eq!(arguments["prompt"], "mock_usage");
        assert_eq!(arguments["schema"]["type"], "object");
        assert_eq!(
            arguments["schema"]["required"],
            serde_json::json!(["result"])
        );
        assert_eq!(
            arguments["schema"]["properties"]["result"]["type"],
            "string"
        );
    }

    #[test]
    fn mock_repeat_read_requests_the_next_unsettled_tool() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_repeat_read 2".to_string());

        let first = mock_call(&conversation);
        assert_eq!(first.tool_calls[0].id, "mock-repeat-read-1-1");
        conversation.add_tool_result(
            "mock-repeat-read-1-1".to_string(),
            "README contents".to_string(),
        );

        let second = mock_call(&conversation);
        assert_eq!(second.tool_calls[0].id, "mock-repeat-read-1-2");
        conversation.add_tool_result(
            "mock-repeat-read-1-2".to_string(),
            "README contents".to_string(),
        );

        let completed = mock_call(&conversation);
        assert!(completed.tool_calls.is_empty());
        assert_eq!(
            completed.assistant_content.as_deref(),
            Some("Mock completed after tool execution.")
        );
    }

    #[test]
    fn mock_repeat_read_scopes_progress_and_ids_to_each_user_request() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_repeat_read 1".to_string());
        let first = mock_call(&conversation);
        assert_eq!(first.tool_calls[0].id, "mock-repeat-read-1-1");
        conversation.add_tool_result(
            "mock-repeat-read-1-1".to_string(),
            "README contents".to_string(),
        );
        conversation.add_user("mock_repeat_read 1".to_string());

        let second = mock_call(&conversation);

        assert_eq!(second.tool_calls[0].id, "mock-repeat-read-2-1");
    }

    #[test]
    fn mock_proposed_plan_returns_tagged_plan_block() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_proposed_plan".to_string());

        let response = mock_call(&conversation);

        assert_eq!(
            response.assistant_content.as_deref(),
            Some(
                "Preface\n<proposed_plan>\n# Final plan\n- first\n- second\n</proposed_plan>\nPostscript"
            )
        );
        assert!(matches!(
            response.steps.first(),
            Some(ProviderStep::MessageDelta(text)) if text.contains("<proposed_plan>")
        ));
    }

    #[test]
    fn mock_provider_can_request_permissions_then_bash() {
        let mut conversation = Conversation::new();
        conversation.add_user(
            "request_permissions_then_bash /tmp/orca-extra :: printf hi > /tmp/orca-extra/out"
                .to_string(),
        );

        let response = mock_call(&conversation);

        assert_eq!(response.tool_calls.len(), 2);
        assert!(matches!(
            response.steps.as_slice(),
            [
                ProviderStep::ToolCall(first),
                ProviderStep::ToolCall(second)
            ] if first.name == ToolName::RequestPermissions && second.name == ToolName::Bash
        ));
    }

    #[test]
    fn mock_force_bash_uses_fresh_tool_ids_across_turns_and_stops_after_result() {
        let mut conversation = Conversation::new();
        conversation.add_user("force_bash printf first".to_string());
        let first = mock_call(&conversation);
        let first_call = first.tool_calls.first().unwrap().clone();
        assert_eq!(first_call.id, "mock-tool-1");
        conversation.add_assistant(None, None, vec![first_call.clone()]);
        conversation.add_tool_result(first_call.id, "first".to_string());
        conversation.add_assistant(Some("first done".to_string()), None, Vec::new());
        conversation.add_user("force_bash printf second".to_string());

        let second = mock_call(&conversation);
        let second_call = second.tool_calls.first().unwrap().clone();
        assert_eq!(second_call.id, "mock-tool-2");
        conversation.add_assistant(None, None, vec![second_call.clone()]);
        conversation.add_tool_result(second_call.id, "second".to_string());

        let completed = mock_call(&conversation);
        assert!(completed.tool_calls.is_empty());
        assert_eq!(
            completed.assistant_content.as_deref(),
            Some("Mock completed after tool execution.")
        );
    }

    #[test]
    fn mock_provider_can_request_network_permissions() {
        let mut conversation = Conversation::new();
        conversation.add_user("request_network_permissions_then_done api.example.com".to_string());

        let response = mock_call(&conversation);

        assert_eq!(response.tool_calls.len(), 1);
        let Some(ProviderStep::ToolCall(request)) = response.steps.first() else {
            panic!("expected request_permissions tool call");
        };
        assert_eq!(request.name, ToolName::RequestPermissions);
        let arguments: serde_json::Value =
            serde_json::from_str(request.raw_arguments.as_deref().unwrap()).unwrap();
        assert_eq!(
            arguments["permissions"]["network"]["domains"]["api.example.com"],
            "allow"
        );
    }

    #[test]
    fn mock_prompt_parses_task_stop() {
        let request = parse_mock_prompt("task_stop task-shell-1").expect("tool request");

        assert_eq!(request.name, ToolName::TaskStop);
        assert_eq!(request.action, ActionKind::Write);
        assert_eq!(request.target.as_deref(), Some("task-shell-1"));
        let arguments: serde_json::Value =
            serde_json::from_str(request.raw_arguments.as_deref().unwrap()).unwrap();
        assert_eq!(arguments["task_id"], "task-shell-1");
    }

    #[test]
    fn mock_prompt_parses_task_list() {
        let request = parse_mock_prompt("task_list").expect("tool request");

        assert_eq!(request.name, ToolName::TaskList);
        assert_eq!(request.action, ActionKind::Read);
        let arguments: serde_json::Value =
            serde_json::from_str(request.raw_arguments.as_deref().unwrap()).unwrap();
        assert_eq!(arguments, serde_json::json!({}));
    }

    #[test]
    fn mock_provider_can_stop_main_session_from_task_list_output() {
        let mut conversation = Conversation::new();
        conversation.add_user("task_stop_main_session".to_string());

        let response = mock_call(&conversation);
        let Some(ProviderStep::ToolCall(request)) = response
            .steps
            .iter()
            .find(|step| matches!(step, ProviderStep::ToolCall(_)))
        else {
            panic!("expected task_list tool call");
        };
        assert_eq!(request.name, ToolName::TaskList);

        conversation.add_assistant(
            None,
            None,
            vec![RawToolCall {
                id: request.id.clone(),
                function_name: request.name.as_str().to_string(),
                arguments: request.raw_arguments.clone().unwrap_or_default(),
            }],
        );
        conversation.add_tool_result(
            request.id.clone(),
            serde_json::json!({
                "tasks": [
                    {
                        "id": "task-main-1",
                        "task_type": "main_session",
                        "status": "running",
                        "subject": "task_stop_main_session"
                    }
                ]
            })
            .to_string(),
        );

        let response = mock_call(&conversation);
        let Some(ProviderStep::ToolCall(request)) = response
            .steps
            .iter()
            .find(|step| matches!(step, ProviderStep::ToolCall(_)))
        else {
            panic!("expected task_stop tool call");
        };
        assert_eq!(request.name, ToolName::TaskStop);
        assert_eq!(request.target.as_deref(), Some("task-main-1"));
        let arguments: serde_json::Value =
            serde_json::from_str(request.raw_arguments.as_deref().unwrap()).unwrap();
        assert_eq!(arguments["task_id"], "task-main-1");
    }

    #[test]
    fn mock_flaky_once_fails_once_then_succeeds() {
        let mut conversation = Conversation::new();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        conversation.add_user(format!("mock_flaky_once {unique}"));

        let first = mock_call(&conversation);
        assert!(matches!(first.steps.first(), Some(ProviderStep::Error(_))));

        let second = mock_call(&conversation);
        assert!(
            second
                .assistant_content
                .as_deref()
                .unwrap_or_default()
                .contains("Mock runtime completed after transient failure")
        );
    }

    #[test]
    fn mock_stream_delay_ms_emits_streaming_deltas_with_delay() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_stream_delay_ms 25".to_string());
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let started = std::time::Instant::now();
        let mut deltas = Vec::new();

        let response = call_streaming(
            ProviderKind::Mock,
            &conversation,
            &config,
            &cancel,
            &mut |step| {
                if let ProviderStep::MessageDelta(text) = step {
                    deltas.push(text.clone());
                }
            },
        );

        assert_eq!(
            deltas,
            vec![
                "Mock slow stream started.".to_string(),
                "Mock slow stream completed.".to_string(),
            ]
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(20),
            "mock stream should delay long enough for TUI concurrency tests"
        );
        assert_eq!(
            response.assistant_content.as_deref(),
            Some("Mock slow stream started.Mock slow stream completed.")
        );
    }

    #[test]
    fn mock_stream_release_marker_waits_for_marker_before_completing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let release_marker = temp.path().join("release");
        let mut conversation = Conversation::new();
        conversation.add_user(format!(
            "mock_stream_release_marker {}",
            release_marker.display()
        ));
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut stream = start_streaming(ProviderKind::Mock, &conversation, &config, cancel);

        let started = stream
            .recv_timeout(Duration::from_secs(1))
            .expect("release-marker stream start");
        assert!(matches!(
            started,
            ProviderStreamEvent::Step(ref delivery)
                if matches!(delivery.step(), ProviderStep::MessageDelta(text)
                    if text == "Mock release-marker stream started.")
        ));
        drop(started);

        assert!(matches!(
            stream.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        std::fs::write(&release_marker, "release").expect("create release marker");

        let completed_delta = stream
            .recv_timeout(Duration::from_secs(1))
            .expect("release-marker completion delta");
        assert!(matches!(
            completed_delta,
            ProviderStreamEvent::Step(ref delivery)
                if matches!(delivery.step(), ProviderStep::MessageDelta(text)
                    if text == "Mock release-marker stream completed.")
        ));
    }

    #[test]
    fn transferable_stream_call_preserves_in_flight_provider_after_first_delta() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_stream_delay_ms 50".to_string());
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut stream = start_streaming(ProviderKind::Mock, &conversation, &config, cancel);

        let first = stream
            .recv_timeout(Duration::from_secs(1))
            .expect("first provider stream event");
        assert!(matches!(
            first,
            ProviderStreamEvent::Step(ref delivery)
                if matches!(delivery.step(), ProviderStep::MessageDelta(text)
                    if text == "Mock slow stream started.")
        ));
        drop(first);

        assert!(matches!(
            stream.recv_timeout(Duration::from_millis(5)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        let second = stream
            .recv_timeout(Duration::from_secs(1))
            .expect("second provider stream event");
        assert!(matches!(
            second,
            ProviderStreamEvent::Step(ref delivery)
                if matches!(delivery.step(), ProviderStep::MessageDelta(text)
                    if text == "Mock slow stream completed.")
        ));
        drop(second);

        let completed = stream
            .recv_timeout(Duration::from_secs(1))
            .expect("provider stream completion");
        assert!(matches!(
            completed,
            ProviderStreamEvent::Completed(ref response)
                if response.assistant_content.as_deref()
                    == Some("Mock slow stream started.Mock slow stream completed.")
        ));
    }

    #[test]
    fn dropping_transferable_stream_call_cancels_and_joins_worker() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_stream_delay_ms 1000".to_string());
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let cancel_probe = cancel.clone();
        let mut stream = start_streaming(ProviderKind::Mock, &conversation, &config, cancel);
        let first = stream
            .recv_timeout(Duration::from_secs(1))
            .expect("first provider stream event");
        assert!(matches!(first, ProviderStreamEvent::Step(_)));
        drop(first);

        let started = std::time::Instant::now();
        drop(stream);

        assert!(cancel_probe.is_cancelled());
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "dropping a transferable call must cancel and join without waiting for the full provider delay"
        );
    }

    #[test]
    fn transferable_stream_cancel_and_join_is_idempotent() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_stream_delay_ms 1000".to_string());
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let cancel_probe = cancel.clone();
        let mut stream = start_streaming(ProviderKind::Mock, &conversation, &config, cancel);

        stream.cancel_and_join();
        stream.cancel_and_join();

        assert!(cancel_probe.is_cancelled());
    }

    #[test]
    fn synchronous_streaming_facade_invokes_callbacks_on_the_calling_thread() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_stream_delay_ms 1".to_string());
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let caller = std::thread::current().id();
        let mut callback_threads = Vec::new();

        let response = call_streaming(
            ProviderKind::Mock,
            &conversation,
            &config,
            &cancel,
            &mut |_| callback_threads.push(std::thread::current().id()),
        );

        assert!(!callback_threads.is_empty());
        assert!(callback_threads.iter().all(|thread| *thread == caller));
        assert!(response.assistant_content.is_some());
    }

    #[test]
    fn synchronous_streaming_facade_callback_panic_cancels_and_joins_worker() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut conversation = Conversation::new();
            conversation.add_user("inspect the repository".to_string());
            let config = ProviderConfig {
                api_key: None,
                base_url: None,
                model: None,
                reasoning_effort: ReasoningEffort::Max,
                tools_override: None,
                mcp_registry: None,
                external_tools: Vec::new(),
            };
            let cancel = CancelToken::new();

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                call_streaming(
                    ProviderKind::DeepSeekFixture,
                    &conversation,
                    &config,
                    &cancel,
                    &mut |_| panic!("callback panic"),
                )
            }));
            let _ = done_tx.send(result.is_err());
        });

        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(500)),
            Ok(true),
            "callback panic must close the step receiver and join the provider worker"
        );
    }

    #[test]
    fn synchronous_streaming_facade_joins_after_mock_cancellation() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_stream_delay_ms 1000".to_string());
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let cancel_from_callback = cancel.clone();
        let started = std::time::Instant::now();

        let response = call_streaming(
            ProviderKind::Mock,
            &conversation,
            &config,
            &cancel,
            &mut |_| cancel_from_callback.cancel(),
        );

        assert!(
            started.elapsed() < Duration::from_millis(250),
            "cancelled mock stream must not wait for the full delay"
        );
        assert_eq!(
            response.assistant_content.as_deref(),
            Some("Mock slow stream started.")
        );
    }

    #[test]
    fn mock_stream_tool_delay_ms_returns_tool_call_after_streaming_delta() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_stream_tool_delay_ms 25 task_list".to_string());
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut deltas = Vec::new();

        let response = call_streaming(
            ProviderKind::Mock,
            &conversation,
            &config,
            &cancel,
            &mut |step| {
                if let ProviderStep::MessageDelta(text) = step {
                    deltas.push(text.clone());
                }
            },
        );

        assert_eq!(deltas, vec!["Mock slow tool stream started.".to_string()]);
        assert_eq!(response.tool_calls.len(), 1);
        assert!(matches!(
            response.steps.iter().find(|step| matches!(step, ProviderStep::ToolCall(_))),
            Some(ProviderStep::ToolCall(request)) if request.name == ToolName::TaskList
        ));
        assert!(response.assistant_content.is_none());
    }

    #[test]
    fn mock_stream_tool_delay_ms_completes_after_tool_result() {
        let mut conversation = Conversation::new();
        conversation.add_user("mock_stream_tool_delay_ms 25 task_list".to_string());
        conversation.add_assistant(
            None,
            None,
            vec![RawToolCall {
                id: "mock-tool-1".to_string(),
                function_name: "task_list".to_string(),
                arguments: "{}".to_string(),
            }],
        );
        conversation.add_tool_result("mock-tool-1".to_string(), "{\"tasks\":[]}".to_string());
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut deltas = Vec::new();

        let response = call_streaming(
            ProviderKind::Mock,
            &conversation,
            &config,
            &cancel,
            &mut |step| {
                if let ProviderStep::MessageDelta(text) = step {
                    deltas.push(text.clone());
                }
            },
        );

        assert_eq!(deltas, vec!["Mock completed after tool execution."]);
        assert!(response.tool_calls.is_empty());
        assert_eq!(
            response.assistant_content.as_deref(),
            Some("Mock completed after tool execution.")
        );
    }

    #[test]
    fn mock_streaming_ignores_tool_results_from_previous_turns() {
        let mut conversation = Conversation::new();
        conversation.add_user("bash true".to_string());
        conversation.add_assistant(
            None,
            None,
            vec![RawToolCall {
                id: "mock-tool-1".to_string(),
                function_name: "bash".to_string(),
                arguments: serde_json::json!({ "command": "true" }).to_string(),
            }],
        );
        conversation.add_tool_result("mock-tool-1".to_string(), "".to_string());
        conversation.add_assistant(Some("first turn completed".to_string()), None, Vec::new());
        conversation.add_user("bash true".to_string());
        let config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let response = call_streaming(
            ProviderKind::Mock,
            &conversation,
            &config,
            &CancelToken::new(),
            &mut |_| {},
        );

        assert!(matches!(
            response.steps.iter().find(|step| matches!(step, ProviderStep::ToolCall(_))),
            Some(ProviderStep::ToolCall(request)) if request.name == ToolName::Bash
        ));
        assert!(response.assistant_content.is_none());
    }
}
