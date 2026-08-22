use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use orca_core::cancel::CancelToken;
use orca_core::conversation::{
    Conversation, ImageDetail, ImageInput, ImageSource, Message, RawToolCall, SummaryState,
    assistant_message_has_payload, normalize_tool_boundaries,
};
use orca_core::provider_types::{
    ProviderError, ProviderErrorKind, ProviderReplayState, ProviderResponse, ProviderStep, Usage,
};
use orca_core::tool_types::{ToolName, ToolRequest};

use crate::ProviderConfig;
use crate::context::render_internal_context;
use crate::tool_schema::{deepseek_strict_tools_schema_for_endpoint, deepseek_tools_schema};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
pub(crate) const DEFAULT_MODEL: &str = "deepseek-v4-flash";
pub(crate) const VISION_MODEL: &str = orca_core::model::VISION_MODEL;
const DEFAULT_CHAT_MAX_TOKENS: u32 = 384_000;
const DEEPSEEK_MAX_TOOLS: usize = 128;
const EMPTY_RESPONSE_RETRIES: usize = 1;
const STREAM_INTEGRITY_RETRIES: usize = 1;
const EMPTY_RESPONSE_ERROR: &str = "response did not contain content or tool calls";
const EMPTY_RESPONSE_RECOVERY_PROMPT: &str = "Continue the current turn. The previous response ended without visible assistant content or tool calls. Return a user-facing answer in content, or call an available tool. Do not return reasoning only.";

#[derive(Debug, Eq, PartialEq)]
struct DeepSeekRequestError {
    kind: ProviderErrorKind,
    message: String,
    usage: Option<Usage>,
}

impl DeepSeekRequestError {
    fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            kind: classify_deepseek_error(&message),
            message,
            usage: None,
        }
    }

    fn with_usage(message: impl Into<String>, usage: Option<Usage>) -> Self {
        let message = message.into();
        Self {
            kind: classify_deepseek_error(&message),
            message,
            usage,
        }
    }

    fn into_provider_error(self) -> (ProviderError, Option<Usage>) {
        (
            ProviderError::new(
                self.kind,
                format!("DeepSeek provider error: {}", self.message),
            ),
            self.usage,
        )
    }
}

impl From<String> for DeepSeekRequestError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl std::fmt::Display for DeepSeekRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn classify_deepseek_error(message: &str) -> ProviderErrorKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("request cancelled") || lower == "cancelled" {
        ProviderErrorKind::Cancelled
    } else if crate::context::is_prompt_too_long_error(message) {
        ProviderErrorKind::ContextExceeded
    } else if lower.contains("429") || lower.contains("rate limit") {
        ProviderErrorKind::RateLimit
    } else if ["500", "502", "503", "504"]
        .into_iter()
        .any(|status| lower.contains(status))
    {
        ProviderErrorKind::Server
    } else if lower.contains("idle read timed out") || lower.contains("timed out") {
        ProviderErrorKind::Timeout
    } else if lower.contains("stream ended before terminal marker") {
        ProviderErrorKind::StreamClosed
    } else if lower.contains("invalid sse data json")
        || lower.contains("invalid utf-8 in sse data")
        || lower.contains("malformed response")
    {
        ProviderErrorKind::MalformedResponse
    } else if lower.contains(EMPTY_RESPONSE_ERROR) {
        ProviderErrorKind::EmptyResponse
    } else if lower.contains("stream read error")
        || lower.contains("response body read failed")
        || lower.contains("error decoding response body")
        || lower.contains("request failed after")
        || lower.contains("failed to build streaming http client")
    {
        ProviderErrorKind::Transport
    } else {
        ProviderErrorKind::Other
    }
}

/// The beta endpoint reports strict-schema rejections as HTTP 400; both retry
/// helpers embed the status in their error strings.
fn is_strict_schema_rejection(error: &str) -> bool {
    error.contains("(400") || error.contains("400 Bad Request")
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    thinking: ThinkingConfig,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<orca_core::config::ReasoningEffort>,
}

#[derive(Debug, Serialize)]
struct ThinkingConfig {
    #[serde(rename = "type")]
    thinking_type: &'static str,
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self {
            thinking_type: "enabled",
        }
    }
}

fn add_empty_response_recovery_instruction(request: &mut ChatRequest) {
    if let Some(last) = request.messages.last_mut()
        && last.role == "user"
        && let Some(content) = &mut last.content
    {
        content.push_str("\n\n");
        content.push_str(EMPTY_RESPONSE_RECOVERY_PROMPT);
        return;
    }

    request.messages.push(ApiMessage {
        role: "user".to_string(),
        content: Some(EMPTY_RESPONSE_RECOVERY_PROMPT.to_string()),
        images: Vec::new(),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    });
}

fn merge_usage(total: &mut Option<Usage>, usage: Option<Usage>) {
    let Some(usage) = usage else {
        return;
    };
    let total = total.get_or_insert_with(Usage::default);
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cache_tokens = total.cache_tokens.saturating_add(usage.cache_tokens);
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug)]
pub(crate) struct ApiMessage {
    pub(crate) role: String,
    pub(crate) content: Option<String>,
    pub(crate) images: Vec<ImageInput>,
    pub(crate) reasoning_content: Option<String>,
    tool_calls: Option<Vec<ApiToolCallRequest>>,
    pub(crate) tool_call_id: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiContentBlock {
    Text { text: String },
    ImageUrl { image_url: ApiImageUrl },
    File { file_id: String },
}

#[derive(Serialize)]
struct ApiImageUrl {
    url: String,
    detail: ImageDetail,
}

impl Serialize for ApiMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("role", &self.role)?;
        if self.images.is_empty() {
            if let Some(content) = &self.content {
                map.serialize_entry("content", content)?;
            }
        } else {
            let mut blocks =
                Vec::with_capacity(self.images.len() + usize::from(self.content.is_some()));
            if let Some(content) = self.content.as_ref().filter(|content| !content.is_empty()) {
                blocks.push(ApiContentBlock::Text {
                    text: content.clone(),
                });
            }
            for image in &self.images {
                blocks.push(match &image.source {
                    ImageSource::Base64 { media_type, data } => ApiContentBlock::ImageUrl {
                        image_url: ApiImageUrl {
                            url: format!("data:{media_type};base64,{data}"),
                            detail: image.detail,
                        },
                    },
                    ImageSource::Url { url } => ApiContentBlock::ImageUrl {
                        image_url: ApiImageUrl {
                            url: url.clone(),
                            detail: image.detail,
                        },
                    },
                    ImageSource::File { file_id } => ApiContentBlock::File {
                        file_id: file_id.clone(),
                    },
                });
            }
            map.serialize_entry("content", &blocks)?;
        }
        if let Some(reasoning_content) = &self.reasoning_content {
            map.serialize_entry("reasoning_content", reasoning_content)?;
        }
        if let Some(tool_calls) = &self.tool_calls {
            map.serialize_entry("tool_calls", tool_calls)?;
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", tool_call_id)?;
        }
        map.end()
    }
}

#[derive(Debug, Serialize)]
struct ApiToolCallRequest {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ApiFunctionRequest,
}

#[derive(Debug, Serialize)]
struct ApiFunctionRequest {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<ApiUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ApiToolCallResponse>>,
}

#[derive(Debug, Deserialize)]
struct ApiToolCallResponse {
    id: String,
    function: ApiFunctionResponse,
}

#[derive(Debug, Deserialize)]
struct ApiFunctionResponse {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct ApiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    prompt_cache_hit_tokens: Option<u64>,
    prompt_cache_miss_tokens: Option<u64>,
}

impl From<ApiUsage> for Usage {
    fn from(usage: ApiUsage) -> Self {
        let input_tokens = usage.prompt_tokens.unwrap_or_else(|| {
            usage.prompt_cache_hit_tokens.unwrap_or(0) + usage.prompt_cache_miss_tokens.unwrap_or(0)
        });
        let output_tokens = usage.completion_tokens.unwrap_or_else(|| {
            usage
                .total_tokens
                .unwrap_or(input_tokens)
                .saturating_sub(input_tokens)
        });
        Self {
            input_tokens,
            output_tokens,
            cache_tokens: usage.prompt_cache_hit_tokens.unwrap_or(0),
        }
    }
}

pub fn call(conversation: &Conversation, config: &ProviderConfig) -> ProviderResponse {
    match request_chat(conversation, config) {
        Ok(response) => response,
        Err(error) => {
            let (error, usage) = error.into_provider_error();
            ProviderResponse {
                steps: vec![ProviderStep::Error(error)],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage,
            }
        }
    }
}

pub async fn call_streaming_async(
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: &CancelToken,
    mut on_step: impl FnMut(&ProviderStep),
) -> ProviderResponse {
    match request_chat_streaming(conversation, config, cancel, &mut on_step).await {
        Ok(response) => response,
        Err(error) => {
            let (error, usage) = error.into_provider_error();
            let step = ProviderStep::Error(error);
            on_step(&step);
            ProviderResponse {
                steps: vec![step],
                assistant_content: None,
                assistant_reasoning: None,
                tool_calls: Vec::new(),
                usage,
            }
        }
    }
}

async fn request_chat_streaming(
    conversation: &Conversation,
    config: &ProviderConfig,
    cancel: &CancelToken,
    on_step: &mut impl FnMut(&ProviderStep),
) -> Result<ProviderResponse, DeepSeekRequestError> {
    let api_key = config.api_key.as_deref().ok_or_else(|| {
        "DEEPSEEK_API_KEY is required (set via env var or ~/.orca/auth.json)".to_string()
    })?;
    let base_url = config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
    let model = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    validate_image_model(conversation, model)?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let streaming_client = crate::http_client::streaming_client()?;

    let messages = conversation_to_api_messages(conversation);
    let definitions = config.tools_override.as_deref().unwrap_or(&[]);
    let tools = cap_tools_for_deepseek(deepseek_tools_schema(definitions));
    let strict_applied = deepseek_strict_tools_schema_for_endpoint(definitions, base_url).is_some();
    let primary_tools = deepseek_primary_request_tools(config);

    let mut request = ChatRequest {
        model: model.to_string(),
        messages,
        thinking: ThinkingConfig::default(),
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        tools: Some(primary_tools),
        max_tokens: Some(DEFAULT_CHAT_MAX_TOKENS),
        reasoning_effort: Some(config.reasoning_effort),
    };

    let mut empty_response_retries = 0;
    let mut stream_integrity_retries = 0;
    let mut suppress_retry_reasoning = false;
    let mut accumulated_usage = None;
    loop {
        let response = match crate::http_client::execute_streaming_with_retry(
            &streaming_client,
            |client| client.post(&url).bearer_auth(api_key).json(&request),
            cancel,
        )
        .await
        {
            Ok(response) => response,
            // Strict mode is Beta; if the server rejects the strict schema, retry
            // once with the plain tool list rather than failing the whole turn.
            Err(error) if strict_applied && is_strict_schema_rejection(&error) => {
                request.tools = Some(tools.clone());
                crate::http_client::execute_streaming_with_retry(
                    &streaming_client,
                    |client| client.post(&url).bearer_auth(api_key).json(&request),
                    cancel,
                )
                .await
                .map_err(|error| DeepSeekRequestError::with_usage(error, accumulated_usage))?
            }
            Err(error) => {
                return Err(DeepSeekRequestError::with_usage(error, accumulated_usage));
            }
        };

        let mut steps = Vec::new();
        let mut emitted_step = false;
        let mut emitted_reasoning = false;

        let stream_result = match crate::streaming::parse_sse_response(
            response,
            cancel,
            crate::http_client::streaming_idle_read_timeout(),
            |delta| {
                let step = provider_step_from_stream_event(delta);
                let is_reasoning_delta = matches!(&step, ProviderStep::ReasoningDelta(_));
                if is_reasoning_delta {
                    emitted_reasoning = true;
                }
                if !(suppress_retry_reasoning && is_reasoning_delta) {
                    emitted_step = true;
                    on_step(&step);
                }
                if stream_step_belongs_in_response_steps(&step) {
                    steps.push(step);
                }
            },
        )
        .await
        {
            Ok(result) => result,
            Err(error)
                if !emitted_step
                    && stream_integrity_retries < STREAM_INTEGRITY_RETRIES
                    && crate::streaming::is_stream_integrity_error(&error) =>
            {
                stream_integrity_retries += 1;
                continue;
            }
            Err(error) => {
                return Err(DeepSeekRequestError::with_usage(error, accumulated_usage));
            }
        };

        merge_usage(&mut accumulated_usage, stream_result.usage);

        match stream_result.finish_reason.as_deref() {
            Some("length") => {
                return Err(DeepSeekRequestError::with_usage(
                    length_finish_reason_error(),
                    accumulated_usage,
                ));
            }
            Some("content_filter") => {
                return Err(DeepSeekRequestError::with_usage(
                    "Response blocked by content filter",
                    accumulated_usage,
                ));
            }
            _ => {}
        }

        let mut raw_calls_for_history = Vec::new();
        for tc in &stream_result.tool_calls {
            raw_calls_for_history.push(RawToolCall {
                id: tc.id.clone(),
                function_name: tc.function_name.clone(),
                arguments: tc.arguments.clone(),
            });

            let tc_response = ApiToolCallResponse {
                id: tc.id.clone(),
                function: ApiFunctionResponse {
                    name: tc.function_name.clone(),
                    arguments: tc.arguments.clone(),
                },
            };
            steps.push(ProviderStep::ToolCall(parse_tool_call(&tc_response)));
        }

        let assistant_reasoning = if stream_result.reasoning.is_empty() {
            None
        } else {
            if !raw_calls_for_history.is_empty() {
                let tool_call_ids: Vec<String> = raw_calls_for_history
                    .iter()
                    .map(|tc| tc.id.clone())
                    .collect();
                steps.push(ProviderStep::ReplayState(ProviderReplayState {
                    provider: "deepseek",
                    reasoning_content: stream_result.reasoning.clone(),
                    tool_call_ids,
                }));
            }
            Some(stream_result.reasoning)
        };

        let assistant_content = if stream_result.content.is_empty() {
            None
        } else {
            Some(stream_result.content)
        };

        if !assistant_message_has_payload(assistant_content.as_deref(), &raw_calls_for_history)
            && !steps
                .iter()
                .any(|step| matches!(step, ProviderStep::Error(_)))
        {
            if empty_response_retries < EMPTY_RESPONSE_RETRIES {
                empty_response_retries += 1;
                suppress_retry_reasoning = emitted_reasoning;
                add_empty_response_recovery_instruction(&mut request);
                continue;
            }
            return Err(DeepSeekRequestError::with_usage(
                EMPTY_RESPONSE_ERROR,
                accumulated_usage,
            ));
        }

        return Ok(ProviderResponse {
            steps,
            assistant_content,
            assistant_reasoning,
            tool_calls: raw_calls_for_history,
            usage: accumulated_usage,
        });
    }
}

fn provider_step_from_stream_event(delta: crate::streaming::StreamEvent<'_>) -> ProviderStep {
    use crate::streaming::StreamEvent;
    match delta {
        StreamEvent::Reasoning(text) => ProviderStep::ReasoningDelta(text.to_string()),
        StreamEvent::Content(text) => ProviderStep::MessageDelta(text.to_string()),
        StreamEvent::ToolCallProgress(progress) => ProviderStep::ToolCallProgress(progress),
    }
}

fn stream_step_belongs_in_response_steps(step: &ProviderStep) -> bool {
    !matches!(step, ProviderStep::ToolCallProgress(_))
}

fn request_chat(
    conversation: &Conversation,
    config: &ProviderConfig,
) -> Result<ProviderResponse, DeepSeekRequestError> {
    let api_key = config.api_key.as_deref().ok_or_else(|| {
        "DEEPSEEK_API_KEY is required (set via env var or ~/.orca/auth.json)".to_string()
    })?;
    let base_url = config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
    let model = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    validate_image_model(conversation, model)?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let messages = conversation_to_api_messages(conversation);
    let definitions = config.tools_override.as_deref().unwrap_or(&[]);
    let tools = cap_tools_for_deepseek(deepseek_tools_schema(definitions));
    let strict_applied = deepseek_strict_tools_schema_for_endpoint(definitions, base_url).is_some();
    let primary_tools = deepseek_primary_request_tools(config);

    let mut request = ChatRequest {
        model: model.to_string(),
        messages,
        thinking: ThinkingConfig::default(),
        stream: false,
        stream_options: None,
        tools: Some(primary_tools),
        max_tokens: Some(DEFAULT_CHAT_MAX_TOKENS),
        reasoning_effort: Some(config.reasoning_effort),
    };

    let mut accumulated_usage = None;
    for empty_attempt in 0..=EMPTY_RESPONSE_RETRIES {
        let response = match crate::http_client::execute_with_retry(|client| {
            client.post(&url).bearer_auth(api_key).json(&request)
        }) {
            Ok(response) => response,
            // Strict mode is Beta; if the server rejects the strict schema, retry
            // once with the plain tool list rather than failing the whole turn.
            Err(error) if strict_applied && is_strict_schema_rejection(&error) => {
                request.tools = Some(tools.clone());
                crate::http_client::execute_with_retry(|client| {
                    client.post(&url).bearer_auth(api_key).json(&request)
                })
                .map_err(|error| DeepSeekRequestError::with_usage(error, accumulated_usage))?
            }
            Err(error) => {
                return Err(DeepSeekRequestError::with_usage(error, accumulated_usage));
            }
        };
        let response = response.json::<ChatResponse>().map_err(|error| {
            DeepSeekRequestError::with_usage(
                format!("invalid response: {error}"),
                accumulated_usage,
            )
        })?;

        let usage = response.usage.map(Usage::from);
        merge_usage(&mut accumulated_usage, usage);
        let Some(choice) = response.choices.into_iter().next() else {
            if empty_attempt < EMPTY_RESPONSE_RETRIES {
                add_empty_response_recovery_instruction(&mut request);
                continue;
            }
            return Err(DeepSeekRequestError::with_usage(
                "response did not contain choices",
                accumulated_usage,
            ));
        };

        let message = choice.message;
        let finish_reason = choice.finish_reason.unwrap_or_default();

        let mut steps = Vec::new();

        match finish_reason.as_str() {
            "length" => {
                return Err(DeepSeekRequestError::with_usage(
                    length_finish_reason_error(),
                    accumulated_usage,
                ));
            }
            "content_filter" => {
                return Err(DeepSeekRequestError::with_usage(
                    "Response blocked by content filter",
                    accumulated_usage,
                ));
            }
            "stop" | "tool_calls" | "" => {}
            other => {
                steps.push(ProviderStep::Error(ProviderError::other(format!(
                    "Unexpected finish_reason: {other}"
                ))));
            }
        }

        let assistant_reasoning = message
            .reasoning_content
            .filter(|text| !text.trim().is_empty());
        let assistant_content = message.content.filter(|text| !text.is_empty());

        if let Some(ref reasoning) = assistant_reasoning {
            steps.push(ProviderStep::ReasoningDelta(reasoning.clone()));
        }

        let raw_tool_calls = message.tool_calls.unwrap_or_default();
        let mut raw_calls_for_history = Vec::new();

        if !raw_tool_calls.is_empty() {
            let tool_call_ids: Vec<String> =
                raw_tool_calls.iter().map(|tc| tc.id.clone()).collect();

            if let Some(ref reasoning) = assistant_reasoning {
                steps.push(ProviderStep::ReplayState(ProviderReplayState {
                    provider: "deepseek",
                    reasoning_content: reasoning.clone(),
                    tool_call_ids,
                }));
            }

            for tc in &raw_tool_calls {
                raw_calls_for_history.push(RawToolCall {
                    id: tc.id.clone(),
                    function_name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                });

                steps.push(ProviderStep::ToolCall(parse_tool_call(tc)));
            }
        }

        if let Some(ref content) = assistant_content {
            steps.push(ProviderStep::MessageDelta(content.clone()));
        }

        if !assistant_message_has_payload(assistant_content.as_deref(), &raw_calls_for_history)
            && !steps
                .iter()
                .any(|step| matches!(step, ProviderStep::Error(_)))
        {
            if empty_attempt < EMPTY_RESPONSE_RETRIES {
                add_empty_response_recovery_instruction(&mut request);
                continue;
            }
            return Err(DeepSeekRequestError::with_usage(
                EMPTY_RESPONSE_ERROR,
                accumulated_usage,
            ));
        }

        return Ok(ProviderResponse {
            steps,
            assistant_content,
            assistant_reasoning,
            tool_calls: raw_calls_for_history,
            usage: accumulated_usage,
        });
    }

    Err(DeepSeekRequestError::with_usage(
        EMPTY_RESPONSE_ERROR,
        accumulated_usage,
    ))
}

fn length_finish_reason_error() -> String {
    "Response truncated: model hit max_tokens limit (finish_reason=length); ask the model to continue in smaller chunks"
        .to_string()
}

fn parse_tool_call(tc: &ApiToolCallResponse) -> ToolRequest {
    let schema_name = tc.function.name.as_str();

    ToolRequest {
        id: tc.id.clone(),
        name: ToolName::from_str(schema_name)
            .unwrap_or_else(|| ToolName::External(schema_name.to_string())),
        action: orca_core::approval_types::ActionKind::Read,
        target: None,
        raw_arguments: Some(tc.function.arguments.clone()),
    }
}

/// Returns the tool list in the primary preflight request. The beta strict
/// payload may fall back to the plain list after a server-side HTTP 400.
pub(crate) fn deepseek_primary_request_tools(config: &ProviderConfig) -> Vec<Value> {
    let definitions = config.tools_override.as_deref().unwrap_or(&[]);
    let base_url = config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
    let plain = cap_tools_for_deepseek(deepseek_tools_schema(definitions));
    deepseek_strict_tools_schema_for_endpoint(definitions, base_url)
        .map(cap_tools_for_deepseek)
        .unwrap_or(plain)
}

fn cap_tools_for_deepseek(mut tools: Vec<Value>) -> Vec<Value> {
    if tools.len() > DEEPSEEK_MAX_TOOLS {
        eprintln!(
            "orca: warning: DeepSeek supports at most {DEEPSEEK_MAX_TOOLS} tools; truncating {} advertised tools",
            tools.len()
        );
        tools.truncate(DEEPSEEK_MAX_TOOLS);
    }
    tools
}

fn replayable_reasoning_content(
    reasoning_content: &Option<String>,
    has_tool_calls: bool,
) -> Option<String> {
    if !has_tool_calls {
        return None;
    }
    reasoning_content
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty() && *text != "(reasoning omitted)")
        .map(str::to_string)
}

pub(crate) fn conversation_to_api_messages(conversation: &Conversation) -> Vec<ApiMessage> {
    let mut messages: Vec<ApiMessage> = Vec::new();
    let mut first_system_done = false;
    let mut safe_messages = conversation.messages.clone();
    normalize_tool_boundaries(&mut safe_messages);

    for msg in &safe_messages {
        let api_msg = match msg {
            Message::System { content, .. } => {
                let result = ApiMessage {
                    role: "system".to_string(),
                    content: Some(content.clone()),
                    images: Vec::new(),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                };
                if !first_system_done {
                    first_system_done = true;
                    messages.push(result);
                    inject_summary_messages(&conversation.summary, &mut messages);
                    continue;
                }
                result
            }
            Message::User {
                content, images, ..
            } => ApiMessage {
                role: "user".to_string(),
                content: Some(content.clone()),
                images: images.clone(),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
            },
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => {
                let api_tool_calls = if tool_calls.is_empty() {
                    None
                } else {
                    Some(
                        tool_calls
                            .iter()
                            .map(|tc| ApiToolCallRequest {
                                id: tc.id.clone(),
                                call_type: "function".to_string(),
                                function: ApiFunctionRequest {
                                    name: tc.function_name.clone(),
                                    arguments: tc.arguments.clone(),
                                },
                            })
                            .collect(),
                    )
                };
                ApiMessage {
                    role: "assistant".to_string(),
                    content: content.clone(),
                    images: Vec::new(),
                    reasoning_content: replayable_reasoning_content(
                        reasoning_content,
                        !tool_calls.is_empty(),
                    ),
                    tool_calls: api_tool_calls,
                    tool_call_id: None,
                }
            }
            Message::Tool {
                tool_call_id,
                content,
                ..
            } => ApiMessage {
                role: "tool".to_string(),
                content: Some(content.clone()),
                images: Vec::new(),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some(tool_call_id.clone()),
            },
        };
        messages.push(api_msg);
    }

    if !first_system_done && !conversation.summary.is_empty() {
        inject_summary_messages(&conversation.summary, &mut messages);
    }

    if let Some(overlay) = render_internal_context(conversation) {
        let insert_at = messages
            .iter()
            .position(|message| message.role == "system")
            .map(|index| index.saturating_add(1))
            .unwrap_or_default();
        messages.insert(
            insert_at,
            ApiMessage {
                role: "system".to_string(),
                content: Some(overlay),
                images: Vec::new(),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
            },
        );
    }

    messages
}

fn validate_image_model(
    conversation: &Conversation,
    model: &str,
) -> Result<(), DeepSeekRequestError> {
    let has_images = conversation.messages.iter().any(|message| {
        matches!(
            message,
            Message::User { images, .. } if !images.is_empty()
        )
    });
    if has_images && model != VISION_MODEL {
        return Err(DeepSeekRequestError::new(format!(
            "model '{model}' does not support image input; use {VISION_MODEL}"
        )));
    }
    Ok(())
}

fn inject_summary_messages(summary: &SummaryState, messages: &mut Vec<ApiMessage>) {
    if let Some(baseline) = &summary.baseline {
        messages.push(ApiMessage {
            role: "system".to_string(),
            content: Some(format!("[Summary baseline]\n{baseline}")),
            images: Vec::new(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
    for (i, delta) in summary.deltas.iter().enumerate() {
        messages.push(ApiMessage {
            role: "system".to_string(),
            content: Some(format!("[Summary update {}]\n{delta}", i + 1)),
            images: Vec::new(),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::TokenCounter;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::ToolName;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    #[test]
    fn deepseek_errors_are_classified_for_attempt_retry_bits_spec_ut() {
        assert_eq!(
            classify_deepseek_error("stream read error: error decoding response body"),
            ProviderErrorKind::Transport
        );
        assert_eq!(
            classify_deepseek_error("stream read error: idle read timed out after 5s"),
            ProviderErrorKind::Timeout
        );
        assert_eq!(
            classify_deepseek_error("stream ended before terminal marker"),
            ProviderErrorKind::StreamClosed
        );
        assert_eq!(
            classify_deepseek_error("invalid SSE data JSON: unexpected EOF"),
            ProviderErrorKind::MalformedResponse
        );
        assert_eq!(
            classify_deepseek_error("request error (429 Too Many Requests): limited"),
            ProviderErrorKind::RateLimit
        );
        assert_eq!(
            classify_deepseek_error("max retries exceeded (last status: 503 Service Unavailable)"),
            ProviderErrorKind::Server
        );
        assert_eq!(
            classify_deepseek_error("prompt_too_long: context length exceeded"),
            ProviderErrorKind::ContextExceeded
        );
    }

    #[test]
    fn vision_user_message_serializes_openai_compatible_image_blocks() {
        let mut conversation = Conversation::new();
        conversation.add_user_with_images(
            "describe this image".to_string(),
            vec![
                ImageInput {
                    source: ImageSource::Base64 {
                        media_type: "image/png".to_string(),
                        data: "aGVsbG8=".to_string(),
                    },
                    detail: ImageDetail::High,
                },
                ImageInput {
                    source: ImageSource::Url {
                        url: "https://example.com/image.webp".to_string(),
                    },
                    detail: ImageDetail::Low,
                },
                ImageInput {
                    source: ImageSource::File {
                        file_id: "file-api-example".to_string(),
                    },
                    detail: ImageDetail::Original,
                },
            ],
        );

        let value = serde_json::to_value(conversation_to_api_messages(&conversation))
            .expect("serialize multimodal messages");
        assert_eq!(value[0]["content"][0]["type"], "text");
        assert_eq!(value[0]["content"][0]["text"], "describe this image");
        assert_eq!(value[0]["content"][1]["type"], "image_url");
        assert_eq!(
            value[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
        assert_eq!(value[0]["content"][1]["image_url"]["detail"], "high");
        assert_eq!(
            value[0]["content"][2]["image_url"]["url"],
            "https://example.com/image.webp"
        );
        assert_eq!(value[0]["content"][2]["image_url"]["detail"], "low");
        assert_eq!(value[0]["content"][3]["type"], "file");
        assert_eq!(value[0]["content"][3]["file_id"], "file-api-example");
    }

    #[test]
    fn image_input_requires_the_vision_model() {
        let mut conversation = Conversation::new();
        conversation.add_user_with_images(
            "inspect".to_string(),
            vec![ImageInput {
                source: ImageSource::Url {
                    url: "https://example.com/image.png".to_string(),
                },
                detail: ImageDetail::High,
            }],
        );

        assert!(validate_image_model(&conversation, VISION_MODEL).is_ok());
        let error = validate_image_model(&conversation, DEFAULT_MODEL)
            .expect_err("text-only model must reject image input");
        assert!(error.to_string().contains(VISION_MODEL));
    }

    fn make_tc(name: &str, arguments: &str) -> ApiToolCallResponse {
        ApiToolCallResponse {
            id: "call_123".to_string(),
            function: ApiFunctionResponse {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn read_http_request_body(stream: &mut std::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        loop {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).expect("read request");
            assert!(read > 0, "client closed before sending full request");
            buffer.extend_from_slice(&chunk[..read]);
            let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .expect("content-length header");
            let body_start = header_end + 4;
            if buffer.len() >= body_start + content_length {
                return String::from_utf8(buffer[body_start..body_start + content_length].to_vec())
                    .expect("request body utf8");
            }
        }
    }

    fn spawn_response_sequence_server(
        responses: Vec<&'static str>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let body = read_http_request_body(&mut stream);
                captured.lock().expect("lock captured bodies").push(body);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                stream.write_all(reply.as_bytes()).expect("write response");
            }
        });
        (base_url, bodies)
    }

    fn spawn_two_response_server(
        first: &'static str,
        second: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        spawn_response_sequence_server(vec![first, second])
    }

    fn incident_plan_boundary_conversation() -> Conversation {
        let mut conversation = Conversation::new();
        conversation.add_user("finish the migration".to_string());
        conversation.add_assistant(
            None,
            Some("The migration is complete; update the plan and report.".to_string()),
            vec![RawToolCall {
                id: "call_update_plan".to_string(),
                function_name: "update_plan".to_string(),
                arguments: r#"{"plan":[{"step":"migrate tools","status":"completed"}]}"#
                    .to_string(),
            }],
        );
        conversation.add_tool_result(
            "call_update_plan".to_string(),
            "Plan updated (1 item). [x] migrate tools".to_string(),
        );
        conversation
    }

    fn spawn_two_streaming_response_server(
        response: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for _ in 0..=EMPTY_RESPONSE_RETRIES {
                let (mut stream, _) = listener.accept().expect("accept request");
                let body = read_http_request_body(&mut stream);
                captured.lock().expect("lock captured bodies").push(body);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                stream.write_all(reply.as_bytes()).expect("write response");
            }
        });
        (base_url, bodies)
    }

    fn spawn_streaming_response_sequence_server(
        responses: Vec<&'static str>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let body = read_http_request_body(&mut stream);
                captured.lock().expect("lock captured bodies").push(body);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                stream.write_all(reply.as_bytes()).expect("write response");
            }
        });
        (base_url, bodies)
    }

    #[test]
    fn tool_call_progress_is_transient_stream_state() {
        let progress = orca_core::provider_types::ToolCallProgress {
            id: "call_1".to_string(),
            function_name: Some("write_file".to_string()),
            arguments_bytes: 8192,
        };

        assert!(!stream_step_belongs_in_response_steps(
            &ProviderStep::ToolCallProgress(progress)
        ));
        assert!(stream_step_belongs_in_response_steps(
            &ProviderStep::MessageDelta("hello".to_string())
        ));
        assert!(stream_step_belongs_in_response_steps(
            &ProviderStep::ReasoningDelta("thinking".to_string())
        ));
    }

    #[test]
    fn parse_read_file() {
        let tc = make_tc("read_file", r#"{"path":"src/main.rs"}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::ReadFile);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
        assert_eq!(req.id, "call_123");
    }

    #[test]
    fn parse_list_files_with_path() {
        let tc = make_tc("list_files", r#"{"path":"src/provider"}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::ListFiles);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
    }

    #[test]
    fn parse_list_files_without_path_defaults_to_dot() {
        let tc = make_tc("list_files", r#"{}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::ListFiles);
        assert!(req.target.is_none());
    }

    #[test]
    fn parse_tool_call_does_not_apply_registry_collision_policy() {
        let tc = make_tc("list_files", r#"{}"#);
        let request = parse_tool_call(&tc);

        assert_eq!(request.name, ToolName::ListFiles);
        assert_eq!(request.action, ActionKind::Read);
        assert!(request.target.is_none());
        assert_eq!(request.raw_arguments.as_deref(), Some(r#"{}"#));
    }

    #[test]
    fn parse_update_plan_leaves_arguments_for_runtime_normalization() {
        let raw = r#"{"plan":[{"completed":true,"step":"a"}]}"#;
        let tc = make_tc("update_plan", raw);
        let request = parse_tool_call(&tc);

        assert_eq!(request.name, ToolName::UpdatePlan);
        assert_eq!(request.raw_arguments.as_deref(), Some(raw));
    }

    #[test]
    fn parse_update_goal_leaves_arguments_for_runtime_normalization() {
        let raw = r#"{"status":"completed","reason":"done"}"#;
        let tc = make_tc("update_goal", raw);
        let request = parse_tool_call(&tc);

        assert_eq!(request.name, ToolName::UpdateGoal);
        assert_eq!(request.raw_arguments.as_deref(), Some(raw));
    }

    #[test]
    fn parse_update_plan_leaves_clean_arguments_untouched() {
        let clean = r#"{"explanation":"x","plan":[{"step":"a","status":"pending"}]}"#;
        let tc = make_tc("update_plan", clean);
        let req = super::parse_tool_call(&tc);
        assert_eq!(req.raw_arguments.as_deref(), Some(clean));
    }

    #[test]
    fn strict_tools_apply_only_on_beta_endpoint_to_eligible_definitions() {
        let definitions = vec![
            crate::tool_schema::ProviderToolDefinition {
                name: "strict_capable".to_string(),
                description: "strict-capable test tool".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "required_value": { "type": "string" },
                        "optional_value": { "type": ["string", "null"] }
                    },
                    "required": ["required_value"],
                    "additionalProperties": false
                }),
                strict_capable: true,
            },
            crate::tool_schema::ProviderToolDefinition {
                name: "non_strict".to_string(),
                description: "ordinary test tool".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } }
                }),
                strict_capable: false,
            },
        ];
        let tools = deepseek_tools_schema(&definitions);

        assert!(
            deepseek_strict_tools_schema_for_endpoint(&definitions, "https://api.deepseek.com")
                .is_none()
        );
        assert!(
            deepseek_strict_tools_schema_for_endpoint(&definitions, DEFAULT_BASE_URL).is_none()
        );

        let strict = deepseek_strict_tools_schema_for_endpoint(
            &definitions,
            "https://api.deepseek.com/beta",
        )
        .expect("beta endpoint must produce a strict tool list");
        let strict_capable = strict
            .iter()
            .find(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str) == Some("strict_capable")
            })
            .expect("strict-capable definition present");
        assert_eq!(
            strict_capable.pointer("/function/strict"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            strict_capable.pointer("/function/parameters/required"),
            Some(&serde_json::json!(["optional_value", "required_value"]))
        );

        let non_strict = strict
            .iter()
            .find(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str) == Some("non_strict")
            })
            .expect("ordinary definition present");
        assert!(
            non_strict.pointer("/function/strict").is_none(),
            "ineligible definitions must stay non-strict"
        );

        let original_strict_capable = tools
            .iter()
            .find(|tool| {
                tool.pointer("/function/name").and_then(Value::as_str) == Some("strict_capable")
            })
            .expect("strict-capable definition present");
        assert!(
            original_strict_capable
                .pointer("/function/strict")
                .is_none()
        );
    }

    #[test]
    fn strict_rejection_detection_matches_retry_helper_error_strings() {
        assert!(is_strict_schema_rejection(
            "request error (400 Bad Request): invalid tools"
        ));
        assert!(is_strict_schema_rejection(
            "request error: HTTP status client error (400 Bad Request) for url (https://api.deepseek.com/beta/chat/completions)"
        ));
        assert!(!is_strict_schema_rejection(
            "max retries exceeded (last status: 500 Internal Server Error)"
        ));
        assert!(!is_strict_schema_rejection(
            "request failed after 3 attempts: connection refused"
        ));
    }

    #[test]
    fn parse_glob_with_pattern_and_path() {
        let tc = make_tc("glob", r#"{"pattern":"**/*.rs","path":"src"}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::Glob);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
        assert_eq!(
            req.raw_arguments.as_deref(),
            Some(r#"{"pattern":"**/*.rs","path":"src"}"#)
        );
    }

    #[test]
    fn parse_glob_with_pattern_only_defaults_path_to_dot() {
        let tc = make_tc("glob", r#"{"pattern":"*.rs"}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::Glob);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
        assert_eq!(req.raw_arguments.as_deref(), Some(r#"{"pattern":"*.rs"}"#));
    }

    #[test]
    fn parse_grep() {
        let tc = make_tc("grep", r#"{"pattern":"fn main","path":"src"}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::Grep);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
    }

    #[test]
    fn parse_bash() {
        let tc = make_tc("bash", r#"{"command":"cargo test"}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::Bash);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
    }

    #[test]
    fn parse_tool_call_does_not_apply_external_action_policy() {
        let raw_arguments = r#"{"command":"cargo test -p orca-provider"}"#;
        let tc = make_tc("bash", raw_arguments);
        let request = parse_tool_call(&tc);

        assert_eq!(request.id, "call_123");
        assert_eq!(request.name, ToolName::Bash);
        assert_eq!(request.action, ActionKind::Read);
        assert!(request.target.is_none());
        assert_eq!(request.raw_arguments.as_deref(), Some(raw_arguments));
    }

    #[test]
    fn parse_edit() {
        let tc = make_tc("edit", r#"{"path":"foo.rs","old_text":"a","new_text":"b"}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::Edit);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_git_status() {
        let tc = make_tc("git_status", r#"{}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::GitStatus);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
    }

    #[test]
    fn parse_subagent() {
        let tc = make_tc(
            "subagent",
            r#"{"description":"inspect repo","prompt":"inspect the repo and report"}"#,
        );
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::Subagent);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_mcp_tool() {
        let tc = make_tc("mcp__demo__search", r#"{"query":"orca"}"#);
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::Mcp("mcp__demo__search".to_string()));
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
        assert_eq!(req.raw_arguments.as_deref(), Some(r#"{"query":"orca"}"#));
    }

    #[test]
    fn parse_web_search() {
        let tc = make_tc(
            "web_search",
            r#"{"query":"deepseek latest","count":3,"fresh_days":30}"#,
        );
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::WebSearch);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_update_plan() {
        let tc = make_tc(
            "update_plan",
            r#"{"plan":[{"step":"Inspect references","status":"completed"},{"step":"Patch Orca","status":"in_progress"}]}"#,
        );
        let req = parse_tool_call(&tc);
        assert_eq!(req.name, ToolName::UpdatePlan);
        assert_eq!(req.action, ActionKind::Read);
        assert!(req.target.is_none());
        assert!(req.raw_arguments.is_some());
    }

    #[test]
    fn parse_unknown_tool_preserves_call_for_model_correction() {
        let tc = make_tc("wc -l", r#"{}"#);
        let request = parse_tool_call(&tc);

        assert_eq!(request.name, ToolName::External("wc -l".to_string()));
        assert_ne!(request.name, ToolName::Bash);
        assert_eq!(request.action, ActionKind::Read);
        assert!(request.target.is_none());
        assert_eq!(request.raw_arguments.as_deref(), Some(r#"{}"#));
    }

    #[test]
    fn parse_unresolved_namespaced_tool_preserves_namespaced_identity() {
        let tc = make_tc("wc__lines", r#"{}"#);
        let request = parse_tool_call(&tc);

        assert_eq!(request.name, ToolName::namespaced("wc", "lines"));
        assert_eq!(request.action, ActionKind::Read);
        assert!(request.target.is_none());
        assert_eq!(request.raw_arguments.as_deref(), Some(r#"{}"#));
    }

    #[test]
    fn parse_namespaced_tool_does_not_apply_external_configuration() {
        let tc = make_tc("acme__deploy", r#"{}"#);
        let request = parse_tool_call(&tc);

        assert_eq!(request.name, ToolName::namespaced("acme", "deploy"));
        assert_eq!(request.action, ActionKind::Read);
        assert!(request.target.is_none());
        assert_eq!(request.raw_arguments.as_deref(), Some(r#"{}"#));
    }

    #[test]
    fn parse_invalid_json_preserves_known_tool_call() {
        let tc = make_tc("write_file", r#"{"path":"note.txt","content":"partial"#);
        let request = super::parse_tool_call(&tc);

        assert_eq!(request.name, ToolName::WriteFile);
        assert!(request.target.is_none());
        assert_eq!(request.raw_arguments, Some(tc.function.arguments));
    }

    #[test]
    fn internal_context_is_a_separate_system_message_after_instructions() {
        let mut conv = Conversation::new();
        conv.add_system("system prompt".to_string());
        conv.add_user("do something".to_string());
        conv.replace_plan_state("[Plan]\n1. step one".to_string());
        conv.replace_goal_state(Some("build a widget".to_string()));

        let messages = conversation_to_api_messages(&conv);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "system");
        let overlay = messages[1].content.as_deref().unwrap();
        assert!(overlay.contains("[Goal state]"));
        assert!(overlay.contains("[Plan]"));
        assert_eq!(messages[2].content.as_deref(), Some("do something"));
    }

    #[test]
    fn internal_context_fragment_respects_its_token_limit() {
        let mut conv = Conversation::new();
        conv.add_system("system prompt".to_string());
        conv.add_user("do something".to_string());
        conv.replace_internal_context(
            "bounded-runtime",
            orca_core::conversation::InternalContextKind::Runtime,
            orca_core::conversation::InternalContextOrigin::System,
            Some("one two three four five six".to_string()),
            2,
        );

        let messages = conversation_to_api_messages(&conv);
        let overlay = messages[1].content.as_deref().unwrap();

        assert!(crate::context::DefaultTokenCounter.count_text(overlay) <= 2);
        assert_ne!(overlay, "one two three four five six");
    }

    #[test]
    fn internal_context_does_not_modify_tool_result() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("read a file".to_string());
        conv.add_assistant(
            None,
            None,
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"x"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "file contents".to_string());
        conv.replace_plan_state("updated plan".to_string());

        let messages = conversation_to_api_messages(&conv);
        assert_eq!(messages.len(), 5);
        let overlay = &messages[1];
        assert_eq!(overlay.role, "system");
        assert!(overlay.content.as_deref().unwrap().contains("updated plan"));
        let last = messages.last().unwrap();
        assert_eq!(last.role, "tool");
        assert_eq!(last.content.as_deref(), Some("file contents"));
    }

    #[test]
    fn no_internal_context_means_no_overlay() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("hello".to_string());

        let messages = conversation_to_api_messages(&conv);
        assert_eq!(messages[1].content.as_deref(), Some("hello"));
    }

    #[test]
    fn api_replay_omits_stale_reasoning_content() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(
            Some("done".to_string()),
            Some("private thinking".to_string()),
            vec![],
        );
        conv.add_user("next".to_string());

        let messages = conversation_to_api_messages(&conv);

        assert!(messages.iter().all(|m| m.reasoning_content.is_none()));
    }

    #[test]
    fn api_messages_drop_reasoning_only_assistant() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(None, Some("private thinking".to_string()), vec![]);
        conv.add_user("second".to_string());

        let messages = conversation_to_api_messages(&conv);

        assert_eq!(
            messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["user", "user"]
        );
    }

    #[test]
    fn api_replay_preserves_reasoning_content_for_tool_call_turns() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(
            None,
            Some("tool reasoning".to_string()),
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"x"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "file contents".to_string());

        let messages = conversation_to_api_messages(&conv);
        let assistant = messages
            .iter()
            .find(|message| message.role == "assistant")
            .expect("assistant replay");

        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("tool reasoning")
        );
    }

    #[test]
    fn api_replay_does_not_send_reasoning_omitted_placeholder() {
        let mut conv = Conversation::new();
        conv.add_user("first".to_string());
        conv.add_assistant(
            None,
            Some("(reasoning omitted)".to_string()),
            vec![RawToolCall {
                id: "tc1".to_string(),
                function_name: "read_file".to_string(),
                arguments: r#"{"path":"x"}"#.to_string(),
            }],
        );
        conv.add_tool_result("tc1".to_string(), "file contents".to_string());

        let messages = conversation_to_api_messages(&conv);

        assert!(messages.iter().all(|m| m.reasoning_content.is_none()));
    }

    #[test]
    fn internal_context_rendering_does_not_mutate_source_messages() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("original text".to_string());
        conv.replace_plan_state("plan data".to_string());

        let _ = conversation_to_api_messages(&conv);
        assert_eq!(conv.messages.len(), 2);
        assert!(
            matches!(&conv.messages[1], Message::User { content, .. } if content == "original text")
        );
    }

    #[test]
    fn api_messages_repair_incomplete_tool_call_boundaries_without_mutating_source() {
        let mut conv = Conversation::new();
        conv.add_system("sys".to_string());
        conv.add_user("start".to_string());
        conv.add_assistant(
            None,
            None,
            vec![
                RawToolCall {
                    id: "tc1".to_string(),
                    function_name: "read_file".to_string(),
                    arguments: r#"{"path":"x"}"#.to_string(),
                },
                RawToolCall {
                    id: "tc2".to_string(),
                    function_name: "grep".to_string(),
                    arguments: r#"{"pattern":"needle"}"#.to_string(),
                },
            ],
        );
        conv.add_tool_result("orphan".to_string(), "discard orphan".to_string());
        conv.add_tool_result("tc2".to_string(), "existing second".to_string());
        conv.add_tool_result("tc2".to_string(), "discard duplicate".to_string());
        conv.add_user("resume after failed turn".to_string());

        let messages = conversation_to_api_messages(&conv);
        let first_payload = serde_json::to_vec(&messages).expect("serialize first payload");
        let second_payload = serde_json::to_vec(&conversation_to_api_messages(&conv))
            .expect("serialize second payload");

        assert_eq!(
            first_payload, second_payload,
            "repair must be deterministic"
        );
        assert_eq!(
            messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["system", "user", "assistant", "tool", "tool", "user"]
        );
        assert_eq!(messages[2].tool_calls.as_ref().map(Vec::len), Some(2));
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("tc1"));
        assert!(
            messages[3]
                .content
                .as_deref()
                .is_some_and(|content| content.contains("indeterminate"))
        );
        assert_eq!(messages[4].tool_call_id.as_deref(), Some("tc2"));
        assert_eq!(messages[4].content.as_deref(), Some("existing second"));
        assert!(messages.iter().all(|message| {
            message.content.as_deref() != Some("discard orphan")
                && message.content.as_deref() != Some("discard duplicate")
        }));
        assert!(matches!(
            &conv.messages[2],
            Message::Assistant { tool_calls, .. } if tool_calls.len() == 2
        ));
    }

    #[test]
    fn chat_request_serializes_latest_reasoning_contract() {
        let request = ChatRequest {
            model: "deepseek-v4-pro".to_string(),
            messages: Vec::new(),
            thinking: ThinkingConfig::default(),
            stream: true,
            stream_options: None,
            tools: None,
            max_tokens: Some(DEFAULT_CHAT_MAX_TOKENS),
            reasoning_effort: Some(orca_core::config::ReasoningEffort::Low),
        };

        let json = serde_json::to_value(request).expect("serialize request");

        assert_eq!(json["reasoning_effort"], "low");
        assert_eq!(json["thinking"]["type"], "enabled");
        assert_eq!(json["max_tokens"], 384_000);
    }

    #[test]
    fn request_chat_retries_once_after_empty_response() {
        let (base_url, bodies) = spawn_two_response_server(
            r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"prompt_cache_hit_tokens":7}}"#,
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":13,"completion_tokens":5,"prompt_cache_hit_tokens":9}}"#,
        );
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let response = request_chat(&conversation, &config).expect("retry succeeds");

        assert_eq!(response.assistant_content.as_deref(), Some("ok"));
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 24,
                output_tokens: 8,
                cache_tokens: 16,
            })
        );
        let bodies = bodies.lock().expect("lock captured bodies");
        assert_eq!(bodies.len(), 2);
        let first: Value = serde_json::from_str(&bodies[0]).expect("first request json");
        let retry: Value = serde_json::from_str(&bodies[1]).expect("retry request json");
        assert_eq!(first["max_tokens"], DEFAULT_CHAT_MAX_TOKENS);
        assert_eq!(retry["max_tokens"], DEFAULT_CHAT_MAX_TOKENS);
        assert_eq!(
            first["messages"].as_array().expect("first messages").len(),
            1
        );
        assert_eq!(
            retry["messages"].as_array().expect("retry messages").len(),
            1
        );
        assert_eq!(
            retry["messages"][0]["content"],
            format!("hello\n\n{EMPTY_RESPONSE_RECOVERY_PROMPT}")
        );
        assert_eq!(conversation.messages.len(), 1);
    }

    #[test]
    fn non_streaming_reasoning_only_response_is_rejected() {
        let reasoning_only = r#"{"choices":[{"message":{"content":null,"reasoning_content":"thinking"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2,"prompt_cache_hit_tokens":5}}"#;
        let (base_url, bodies) =
            spawn_response_sequence_server(vec![reasoning_only; EMPTY_RESPONSE_RETRIES + 1]);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let error = request_chat(&conversation, &config).expect_err("reasoning-only is invalid");

        assert_eq!(error.message, EMPTY_RESPONSE_ERROR);
        assert_eq!(
            error.usage,
            Some(Usage {
                input_tokens: 14,
                output_tokens: 4,
                cache_tokens: 10,
            })
        );
        assert_eq!(
            bodies.lock().expect("lock captured bodies").len(),
            EMPTY_RESPONSE_RETRIES + 1
        );
    }

    #[test]
    fn non_streaming_length_finish_reason_is_a_truncation_error() {
        // finish_reason=length means the model hit max_tokens. It is a terminal
        // error (no retry) that must surface the truncation guidance verbatim.
        let truncated = r#"{"choices":[{"message":{"content":"partial answer"},"finish_reason":"length"}],"usage":{"prompt_tokens":9,"completion_tokens":6,"prompt_cache_hit_tokens":4}}"#;
        let (base_url, bodies) = spawn_response_sequence_server(vec![truncated]);
        let mut conversation = Conversation::new();
        conversation.add_user("write a long essay".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let error = request_chat(&conversation, &config).expect_err("length must be an error");

        assert_eq!(error.message, length_finish_reason_error());
        // Usage from the truncated response is preserved so callers still bill it.
        assert_eq!(
            error.usage,
            Some(Usage {
                input_tokens: 9,
                output_tokens: 6,
                cache_tokens: 4,
            })
        );
        // Terminal error: exactly one request, no retry.
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
    }

    #[test]
    fn non_streaming_content_filter_finish_reason_is_an_error() {
        let filtered = r#"{"choices":[{"message":{"content":""},"finish_reason":"content_filter"}],"usage":{"prompt_tokens":8,"completion_tokens":0,"prompt_cache_hit_tokens":3}}"#;
        let (base_url, bodies) = spawn_response_sequence_server(vec![filtered]);
        let mut conversation = Conversation::new();
        conversation.add_user("blocked prompt".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let error =
            request_chat(&conversation, &config).expect_err("content_filter must be an error");

        assert_eq!(error.message, "Response blocked by content filter");
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
    }

    #[test]
    fn non_streaming_tool_call_without_reasoning_does_not_fabricate_replay_state() {
        let missing_reasoning = r#"{"choices":[{"message":{"content":null,"reasoning_content":"   ","tool_calls":[{"id":"call_missing_reasoning","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let (base_url, _bodies) = spawn_response_sequence_server(vec![missing_reasoning]);
        let mut conversation = Conversation::new();
        conversation.add_user("read the file".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-pro".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let response = request_chat(&conversation, &config)
            .expect("tool calls without server-provided reasoning remain executable");

        assert!(response.steps.iter().any(|step| matches!(
            step,
            ProviderStep::ToolCall(request) if request.id == "call_missing_reasoning"
        )));
        assert!(
            response
                .steps
                .iter()
                .all(|step| !matches!(step, ProviderStep::ReplayState(_)))
        );
        assert_eq!(response.assistant_reasoning, None);
    }

    #[test]
    fn non_streaming_facade_preserves_usage_when_recovery_also_fails() {
        let first = r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1,"prompt_cache_hit_tokens":2}}"#;
        let second = r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"prompt_cache_hit_tokens":4}}"#;
        let (base_url, bodies) = spawn_two_response_server(first, second);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let response = crate::call(
            orca_core::config::ProviderKind::DeepSeek,
            &conversation,
            &config,
        );

        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::Error(message)]
                if message.message
                    == "DeepSeek provider error: response did not contain choices"
        ));
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 8,
                output_tokens: 3,
                cache_tokens: 6,
            })
        );
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 2);
    }

    #[test]
    fn non_streaming_unknown_tool_is_returned_for_model_correction() {
        let unknown = r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call_wc","function":{"name":"wc -l","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#;
        let (base_url, bodies) = spawn_two_response_server(unknown, unknown);
        let mut conversation = Conversation::new();
        conversation.add_user("count lines".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-pro".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };

        let response = request_chat(&conversation, &config)
            .expect("unknown tool should remain a corrective tool turn");

        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
        assert!(
            response
                .steps
                .iter()
                .all(|step| !matches!(step, ProviderStep::Error(_)))
        );
        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::ToolCall(request)]
                if request.id == "call_wc"
                    && request.name == ToolName::External("wc -l".to_string())
                    && request.name != ToolName::Bash
                    && request.action == ActionKind::Read
                    && request.target.is_none()
                    && request.raw_arguments.as_deref() == Some("{}")
        ));
        assert_eq!(response.tool_calls[0].id, "call_wc");
        assert_eq!(response.tool_calls[0].function_name, "wc -l");
        assert_eq!(response.tool_calls[0].arguments, "{}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_reasoning_only_response_is_rejected() {
        let reasoning_only = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":null}]}\n\n\
                              data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                              data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\"prompt_cache_hit_tokens\":7}}\n\n\
                              data: [DONE]\n\n";
        let (base_url, bodies) = spawn_two_streaming_response_server(reasoning_only);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let error = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect_err("reasoning-only is invalid");

        assert_eq!(error.message, EMPTY_RESPONSE_ERROR);
        assert_eq!(
            error.usage,
            Some(Usage {
                input_tokens: 22,
                output_tokens: 6,
                cache_tokens: 14,
            })
        );
        assert_eq!(
            bodies.lock().expect("lock captured bodies").len(),
            EMPTY_RESPONSE_RETRIES + 1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_length_finish_reason_is_a_truncation_error() {
        // A streamed finish_reason=length must terminate as the same truncation
        // error as the non-streaming path, carrying the streamed usage.
        let truncated = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n\
                         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                         data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":8,\"prompt_cache_hit_tokens\":6}}\n\n\
                         data: [DONE]\n\n";
        let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![truncated]);
        let mut conversation = Conversation::new();
        conversation.add_user("write a long essay".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let error = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect_err("length must be an error");

        assert_eq!(error.message, length_finish_reason_error());
        assert_eq!(
            error.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 8,
                cache_tokens: 6,
            })
        );
        // Terminal error: exactly one request, no retry.
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_content_filter_finish_reason_is_an_error() {
        let filtered = "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n\
                        data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":0,\"prompt_cache_hit_tokens\":2}}\n\n\
                        data: [DONE]\n\n";
        let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![filtered]);
        let mut conversation = Conversation::new();
        conversation.add_user("blocked prompt".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let error = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect_err("content_filter must be an error");

        assert_eq!(error.message, "Response blocked by content filter");
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_facade_preserves_usage_when_recovery_also_fails() {
        let first = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":13,\"completion_tokens\":4,\"prompt_cache_hit_tokens\":8}}\n\n\
                     data: [DONE]\n\n";
        let second = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":5,\"prompt_cache_hit_tokens\":9}}\n\n\
                      data: [DONE]\n\n";
        let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![first, second]);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut emitted = Vec::new();

        let response = crate::call_streaming_async(
            orca_core::config::ProviderKind::DeepSeek,
            &conversation,
            &config,
            &cancel,
            |step| emitted.push(step.clone()),
        )
        .await;

        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::Error(message)]
                if message.message
                    == "DeepSeek provider error: response did not contain content or tool calls"
        ));
        assert!(matches!(
            emitted.as_slice(),
            [ProviderStep::Error(message)]
                if message.message
                    == "DeepSeek provider error: response did not contain content or tool calls"
        ));
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 30,
                output_tokens: 9,
                cache_tokens: 17,
            })
        );
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_empty_response_retry_adds_recovery_instruction() {
        let reasoning_only = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"first attempt thinking\"},\"finish_reason\":null}]}\n\n\
                              data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                              data: {\"choices\":[],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":4,\"prompt_cache_hit_tokens\":12}}\n\n\
                              data: [DONE]\n\n";
        let recovered = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"retry thinking\"},\"finish_reason\":null}]}\n\n\
                         data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"},\"finish_reason\":null}]}\n\n\
                         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                         data: {\"choices\":[],\"usage\":{\"prompt_tokens\":19,\"completion_tokens\":6,\"prompt_cache_hit_tokens\":14}}\n\n\
                         data: [DONE]\n\n";
        let (base_url, bodies) =
            spawn_streaming_response_sequence_server(vec![reasoning_only, recovered]);
        let conversation = incident_plan_boundary_conversation();
        let original_messages = serde_json::to_value(conversation_to_api_messages(&conversation))
            .expect("serialize original messages");
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut emitted = Vec::new();

        let response = request_chat_streaming(&conversation, &config, &cancel, &mut |step| {
            emitted.push(step.clone())
        })
        .await
        .expect("recovery response succeeds");

        assert_eq!(response.assistant_content.as_deref(), Some("recovered"));
        assert_eq!(
            response.usage,
            Some(Usage {
                input_tokens: 36,
                output_tokens: 10,
                cache_tokens: 26,
            })
        );
        assert!(emitted.iter().any(
            |step| matches!(step, ProviderStep::ReasoningDelta(text) if text == "first attempt thinking")
        ));
        assert!(!emitted.iter().any(
            |step| matches!(step, ProviderStep::ReasoningDelta(text) if text == "retry thinking")
        ));
        assert!(
            emitted.iter().any(
                |step| matches!(step, ProviderStep::MessageDelta(text) if text == "recovered")
            )
        );
        let bodies = bodies.lock().expect("lock captured bodies");
        assert_eq!(bodies.len(), 2);
        let first: Value = serde_json::from_str(&bodies[0]).expect("first request json");
        let retry: Value = serde_json::from_str(&bodies[1]).expect("retry request json");
        assert_eq!(
            first["messages"].as_array().expect("first messages").len(),
            3
        );
        assert_eq!(
            retry["messages"].as_array().expect("retry messages").len(),
            4
        );
        assert_eq!(
            first["messages"][1]["reasoning_content"],
            "The migration is complete; update the plan and report."
        );
        assert_eq!(retry["messages"][2]["role"], "tool");
        assert!(!bodies[1].contains("first attempt thinking"));
        assert_eq!(
            retry["messages"][3]["content"],
            EMPTY_RESPONSE_RECOVERY_PROMPT
        );
        assert_eq!(
            serde_json::to_value(conversation_to_api_messages(&conversation))
                .expect("serialize unchanged messages"),
            original_messages
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_invalid_tool_arguments_are_returned_for_tool_failure() {
        let incomplete = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_incomplete\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\",\\\"content\\\":\\\"partial\"}}]},\"finish_reason\":null}]}\n\n\
                          data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                          data: [DONE]\n\n";
        let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![incomplete]);
        let mut conversation = Conversation::new();
        conversation.add_user("write the file".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-pro".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let response = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect("invalid tool arguments should remain a tool call");

        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::ToolCall(request)]
                if request.id == "call_incomplete"
                    && request.target.is_none()
                    && request.raw_arguments.as_deref()
                        == Some("{\"path\":\"src/main.rs\",\"content\":\"partial")
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_tool_call_without_reasoning_does_not_fabricate_replay_state() {
        let missing_reasoning = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_missing_reasoning\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
                                 data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                                 data: [DONE]\n\n";
        let (base_url, _bodies) = spawn_streaming_response_sequence_server(vec![missing_reasoning]);
        let mut conversation = Conversation::new();
        conversation.add_user("read the file".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let response = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect("tool calls without server-provided reasoning remain executable");

        assert!(response.steps.iter().any(|step| matches!(
            step,
            ProviderStep::ToolCall(request) if request.id == "call_missing_reasoning"
        )));
        assert!(
            response
                .steps
                .iter()
                .all(|step| !matches!(step, ProviderStep::ReplayState(_)))
        );
        assert_eq!(response.assistant_reasoning, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_unknown_tool_is_returned_for_model_correction() {
        let unknown = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_wc\",\"function\":{\"name\":\"wc -l\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n\
                       data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                       data: [DONE]\n\n";
        let (base_url, bodies) = spawn_streaming_response_sequence_server(vec![unknown]);
        let mut conversation = Conversation::new();
        conversation.add_user("count lines".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-pro".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let response = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect("unknown tool should remain a corrective tool turn");

        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
        assert!(
            response
                .steps
                .iter()
                .all(|step| !matches!(step, ProviderStep::Error(_)))
        );
        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::ToolCall(request)]
                if request.id == "call_wc"
                    && request.name == ToolName::External("wc -l".to_string())
                    && request.target.is_none()
        ));
        assert_eq!(response.tool_calls[0].function_name, "wc -l");
        assert_eq!(response.tool_calls[0].arguments, "{}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_premature_eof_without_visible_delta_retries_once() {
        let premature = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":0,\"total_tokens\":1}}\n\n";
        let complete = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_complete\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\",\\\"content\\\":\\\"done\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                        data: [DONE]\n\n";
        let (base_url, bodies) =
            spawn_streaming_response_sequence_server(vec![premature, complete]);
        let mut conversation = Conversation::new();
        conversation.add_user("write the file".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-pro".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();

        let response = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {})
            .await
            .expect("premature stream should retry once");

        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 2);
        assert!(matches!(
            response.steps.as_slice(),
            [ProviderStep::ToolCall(request)]
                if request.id == "call_complete"
                    && request.name == ToolName::WriteFile
                    && request.target.is_none()
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_integrity_error_after_visible_delta_does_not_retry() {
        let premature = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
        let replacement = "data: {\"choices\":[{\"delta\":{\"content\":\"replacement\"},\"finish_reason\":\"stop\"}]}\n\n\
                           data: [DONE]\n\n";
        let (base_url, bodies) =
            spawn_streaming_response_sequence_server(vec![premature, replacement]);
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let mut deltas = Vec::new();

        let error = request_chat_streaming(&conversation, &config, &cancel, &mut |step| {
            if let ProviderStep::MessageDelta(text) = step {
                deltas.push(text.clone());
            }
        })
        .await
        .expect_err("a visible partial response must not be replayed transparently");

        assert_eq!(error.message, "stream ended before terminal marker");
        assert_eq!(error.usage, None);
        assert_eq!(bodies.lock().expect("lock captured bodies").len(), 1);
        assert_eq!(deltas, vec!["partial"]);
    }

    #[test]
    fn synchronous_facade_cancellation_does_not_deliver_prefetched_deltas() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind facade stream server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept facade stream request");
            let _ = read_http_request_body(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"first\"},\"finish_reason\":null}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"second\"},\"finish_reason\":null}]}\n\n",
                )
                .expect("write facade stream response");
            stream.flush().expect("flush facade stream response");
            stream
                .set_read_timeout(Some(Duration::from_millis(400)))
                .expect("set facade peer close timeout");
            let mut byte = [0_u8; 1];
            let closed = match stream.read(&mut byte) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => panic!("read facade client close: {error}"),
            };
            closed_tx.send(closed).expect("report facade close");
        });
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let cancel_from_callback = cancel.clone();
        let mut deltas = Vec::new();
        let started = Instant::now();

        let _response = crate::call_streaming(
            orca_core::config::ProviderKind::DeepSeek,
            &conversation,
            &config,
            &cancel,
            &mut |step| {
                if let ProviderStep::MessageDelta(text) = step {
                    deltas.push(text.clone());
                    if deltas.len() == 1 {
                        std::thread::sleep(Duration::from_millis(50));
                        cancel_from_callback.cancel();
                    }
                }
            },
        );
        let elapsed = started.elapsed();
        let connection_closed = closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("wait for facade peer close result");

        server.join().expect("facade stream server");
        assert_eq!(deltas, vec!["first"]);
        assert!(
            elapsed < Duration::from_millis(500),
            "cancelled facade returned after {elapsed:?}"
        );
        assert!(connection_closed, "facade cancellation must close the peer");
    }

    #[test]
    fn synchronous_facade_cancellation_stops_remaining_same_frame_callbacks() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind facade stream server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept facade stream request");
            let _ = read_http_request_body(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"first\",\"content\":\"second\"},\"finish_reason\":null}]}\n\n",
                )
                .expect("write facade stream response");
            stream.flush().expect("flush facade stream response");
            stream
                .set_read_timeout(Some(Duration::from_millis(400)))
                .expect("set facade peer close timeout");
            let mut byte = [0_u8; 1];
            let closed = match stream.read(&mut byte) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => panic!("read facade client close: {error}"),
            };
            closed_tx.send(closed).expect("report facade close");
        });
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let cancel_from_callback = cancel.clone();
        let mut deltas = Vec::new();
        let started = Instant::now();

        let _response = crate::call_streaming(
            orca_core::config::ProviderKind::DeepSeek,
            &conversation,
            &config,
            &cancel,
            &mut |step| {
                let text = match step {
                    ProviderStep::ReasoningDelta(text) | ProviderStep::MessageDelta(text) => text,
                    _ => return,
                };
                deltas.push(text.clone());
                if deltas.len() == 1 {
                    cancel_from_callback.cancel();
                }
            },
        );
        let elapsed = started.elapsed();
        let connection_closed = closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("wait for facade peer close result");

        server.join().expect("facade stream server");
        assert_eq!(deltas, vec!["first"]);
        assert!(
            elapsed < Duration::from_millis(500),
            "cancelled facade returned after {elapsed:?}"
        );
        assert!(connection_closed, "facade cancellation must close the peer");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_streaming_body_closes_in_flight_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled stream server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (headers_tx, headers_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept stalled stream request");
            let _ = read_http_request_body(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .expect("write stream headers");
            stream.flush().expect("flush stream headers");
            headers_tx.send(()).expect("announce stream headers");
            stream
                .set_read_timeout(Some(Duration::from_millis(400)))
                .expect("set peer close timeout");
            let mut byte = [0_u8; 1];
            let closed = match stream.read(&mut byte) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    false
                }
                Err(error) => panic!("read stalled stream close: {error}"),
            };
            closed_tx.send(closed).expect("report stream close");
        });
        let mut conversation = Conversation::new();
        conversation.add_user("hello".to_string());
        let config = ProviderConfig {
            api_key: Some("test-key".to_string()),
            base_url: Some(base_url),
            model: Some("deepseek-v4-flash".to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::default(),
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let cancel = CancelToken::new();
        let cancel_after_headers = cancel.clone();
        let canceller = std::thread::spawn(move || {
            headers_rx.recv().expect("wait for stream headers");
            std::thread::sleep(Duration::from_millis(100));
            cancel_after_headers.cancel();
        });

        let started = Instant::now();
        let result = request_chat_streaming(&conversation, &config, &cancel, &mut |_| {}).await;
        let elapsed = started.elapsed();
        let connection_closed = closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("wait for stalled stream close result");

        canceller.join().expect("stalled stream canceller");
        server.join().expect("stalled stream server");
        let error = result.unwrap_err();
        assert_eq!(error.message, "cancelled");
        assert_eq!(error.usage, None);
        assert!(
            elapsed < Duration::from_millis(500),
            "cancelled stream returned after {elapsed:?}"
        );
        assert!(
            connection_closed,
            "cancelled stream left the response body owned by a detached reader"
        );
    }

    #[test]
    fn deepseek_tools_are_capped_at_api_limit() {
        let tools = (0..(DEEPSEEK_MAX_TOOLS + 5))
            .map(|index| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": format!("tool_{index}"),
                        "description": "test",
                        "parameters": {"type": "object", "properties": {}, "additionalProperties": false}
                    }
                })
            })
            .collect::<Vec<_>>();

        let capped = cap_tools_for_deepseek(tools);

        assert_eq!(capped.len(), DEEPSEEK_MAX_TOOLS);
        assert_eq!(
            capped[DEEPSEEK_MAX_TOOLS - 1]["function"]["name"],
            "tool_127"
        );
    }

    #[test]
    fn primary_tool_payload_sorts_before_capping_for_plain_and_strict_requests() {
        let definitions = (0..(DEEPSEEK_MAX_TOOLS + 5))
            .rev()
            .map(|index| crate::tool_schema::ProviderToolDefinition {
                name: format!("tool_{index:03}"),
                description: "test".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
                strict_capable: true,
            })
            .collect::<Vec<_>>();
        let mut permuted = definitions.clone();
        permuted.reverse();

        let plain = ProviderConfig {
            api_key: None,
            base_url: Some("https://api.deepseek.com".to_string()),
            model: None,
            reasoning_effort: Default::default(),
            tools_override: Some(definitions.clone()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let permuted_plain = ProviderConfig {
            tools_override: Some(permuted.clone()),
            ..plain.clone()
        };
        let plain_tools = deepseek_primary_request_tools(&plain);
        assert_eq!(plain_tools, deepseek_primary_request_tools(&permuted_plain));
        assert_eq!(plain_tools.len(), DEEPSEEK_MAX_TOOLS);
        assert_eq!(plain_tools[0]["function"]["name"], "tool_000");
        assert_eq!(plain_tools[127]["function"]["name"], "tool_127");

        let strict = ProviderConfig {
            base_url: Some("https://api.deepseek.com/beta".to_string()),
            ..plain
        };
        let permuted_strict = ProviderConfig {
            tools_override: Some(permuted),
            ..strict.clone()
        };
        let strict_tools = deepseek_primary_request_tools(&strict);
        assert_eq!(
            strict_tools,
            deepseek_primary_request_tools(&permuted_strict)
        );
        assert_eq!(strict_tools.len(), DEEPSEEK_MAX_TOOLS);
        assert_eq!(strict_tools[0]["function"]["name"], "tool_000");
        assert_eq!(strict_tools[127]["function"]["name"], "tool_127");
        assert_eq!(strict_tools[0]["function"]["strict"], true);
    }
}
