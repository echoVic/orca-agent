use serde::{Deserialize, Serialize};

use crate::conversation::RawToolCall;
use crate::tool_types::ToolRequest;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
}

impl Usage {
    pub fn is_empty(self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0 && self.cache_tokens == 0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderReplayState {
    pub provider: &'static str,
    pub reasoning_content: String,
    pub tool_call_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCallProgress {
    pub id: String,
    pub function_name: Option<String>,
    pub arguments_bytes: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Transport,
    Timeout,
    StreamClosed,
    MalformedResponse,
    ContextExceeded,
    Server,
    RateLimit,
    EmptyResponse,
    Cancelled,
    Other,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Other, message)
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::Transport
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::Server
                | ProviderErrorKind::RateLimit
                | ProviderErrorKind::EmptyResponse
        )
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Debug)]
pub enum ProviderStep {
    ReasoningDelta(String),
    MessageDelta(String),
    ToolCallProgress(ToolCallProgress),
    ToolCall(ToolRequest),
    ReplayState(ProviderReplayState),
    Error(ProviderError),
}

impl ProviderResponse {
    pub fn error(&self) -> Option<&ProviderError> {
        self.steps.iter().find_map(|step| match step {
            ProviderStep::Error(error) => Some(error),
            _ => None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ProviderResponse {
    pub steps: Vec<ProviderStep>,
    pub assistant_content: Option<String>,
    pub assistant_reasoning: Option<String>,
    pub tool_calls: Vec<RawToolCall>,
    pub usage: Option<Usage>,
}
