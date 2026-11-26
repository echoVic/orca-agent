use serde::{Deserialize, Serialize};
use std::time::Duration;

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

/// Whether the `error` field of a provider or tool API response holds an
/// error. Some gateways attach one to every stream chunk: `null`, `false`,
/// `0`, a blank string and an empty object or array say nothing went wrong.
pub fn error_field_reports_error(error: &serde_json::Value) -> bool {
    match error {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(flag) => *flag,
        serde_json::Value::Number(number) => number.as_f64() != Some(0.0),
        serde_json::Value::String(text) => !text.trim().is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        serde_json::Value::Object(fields) => !fields.is_empty(),
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
pub struct PartialReply {
    pub content: String,
    pub reasoning: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_reply: Option<PartialReply>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self::with_http(kind, message, None, None)
    }

    pub fn with_http(
        kind: ProviderErrorKind,
        message: impl Into<String>,
        http_status: Option<u16>,
        request_id: Option<String>,
    ) -> Self {
        Self {
            kind,
            message: crate::redaction::redact_sensitive_text(&message.into()),
            http_status,
            request_id,
            partial_reply: None,
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Other, message)
    }

    pub fn is_retryable(&self) -> bool {
        // The runtime discards an incomplete provider attempt before retrying,
        // so a stream that closes before its terminal marker is replay-safe.
        matches!(
            self.kind,
            ProviderErrorKind::Transport
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::StreamClosed
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderRetry {
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    pub kind: ProviderErrorKind,
    pub reason: String,
}

impl ProviderRetry {
    pub fn new(attempt: u32, max_attempts: u32, delay: Duration, error: &ProviderError) -> Self {
        Self {
            attempt,
            max_attempts,
            delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            kind: error.kind,
            reason: error.message.chars().take(200).collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProviderStep {
    /// Live retry state. `None` marks the start of the next attempt.
    Retry(Option<ProviderRetry>),
    /// Live usage of the successful request when the response includes earlier costs.
    AttemptUsage(Option<Usage>),
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ProviderError, ProviderErrorKind, error_field_reports_error};

    #[test]
    fn provider_error_json_only_grows() {
        #[derive(serde::Deserialize)]
        struct LegacyProviderError {
            kind: ProviderErrorKind,
            message: String,
        }

        let plain = serde_json::to_value(ProviderError::other("x")).unwrap();
        assert_eq!(plain, json!({"kind": "other", "message": "x"}));
        assert_eq!(
            serde_json::from_value::<ProviderError>(plain).unwrap(),
            ProviderError::other("x")
        );

        let with_http = json!({
            "kind": "server", "message": "unavailable", "http_status": 503, "request_id": "req-1"
        });
        let error: ProviderError = serde_json::from_value(with_http.clone()).unwrap();
        let serialized = serde_json::to_value(error).unwrap();
        assert_eq!(serialized, with_http);
        let legacy: LegacyProviderError = serde_json::from_value(serialized).unwrap();
        assert_eq!(legacy.kind, ProviderErrorKind::Server);
        assert_eq!(legacy.message, "unavailable");
    }

    #[test]
    fn error_field_reports_error_only_for_real_errors() {
        for empty in [
            json!(null),
            json!(false),
            json!(0),
            json!(0.0),
            json!(""),
            json!("  "),
            json!({}),
            json!([]),
        ] {
            assert!(!error_field_reports_error(&empty), "{empty} is no error");
        }
        for error in [
            json!(true),
            json!(1),
            json!(-1),
            json!("boom"),
            json!({"message": "x"}),
            json!([1]),
        ] {
            assert!(error_field_reports_error(&error), "{error} is an error");
        }
    }
}
