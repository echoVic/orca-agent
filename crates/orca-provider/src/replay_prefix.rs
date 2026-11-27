use std::io::{self, Write};

use orca_core::conversation::{
    CapturedProviderMessage, CapturedProviderRequest, Conversation, Message,
    normalize_tool_boundaries,
};
use orca_core::model::{ModelRouteContext, ModelSelection, PRO_MODEL, accepts_tool_images};
use orca_core::subagent_types::SubagentType;
use serde_json::Value;
use serde_json::value::{RawValue, to_raw_value};
use sha2::{Digest, Sha256};

use crate::ProviderConfig;
use crate::context::DefaultTokenCounter;
use crate::deepseek_http::{
    ApiMessage, DEFAULT_BASE_URL, deepseek_plain_request_tools, deepseek_primary_request_tools,
};

pub(crate) fn model(config: &ProviderConfig) -> String {
    ModelSelection::from_unchecked(config.model.clone())
        .route(ModelRouteContext {
            subagent_type: &SubagentType::General,
            subagent_model: None,
            has_images: false,
        })
        .actual_model
}

fn endpoint(config: &ProviderConfig) -> &str {
    config
        .base_url
        .as_deref()
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
}

#[derive(Default)]
struct MessageDigest(Sha256);

impl Write for MessageDigest {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn fingerprint(message: &Message, include_images: bool) -> Option<[u8; 32]> {
    let mut hash = MessageDigest::default();
    let images = if include_images {
        message.images()
    } else {
        &[]
    };
    match message {
        Message::System { content, pinned } => {
            serde_json::to_writer(&mut hash, &("system", content, pinned))
        }
        Message::User {
            content, pinned, ..
        } => serde_json::to_writer(&mut hash, &("user", content, pinned, images)),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            cut_short,
            pinned,
        } => serde_json::to_writer(
            &mut hash,
            &(
                "assistant",
                content,
                reasoning_content,
                tool_calls,
                cut_short,
                pinned,
            ),
        ),
        Message::Tool {
            tool_call_id,
            content,
            pinned,
            ..
        } => serde_json::to_writer(&mut hash, &("tool", tool_call_id, content, pinned, images)),
    }
    .ok()?;
    Some(hash.0.finalize().into())
}

/// Binds the prepared ordinary request back to the live conversation. Image
/// routing may clear pixels in place. If it removes or rewrites a message, we
/// can only map boundaries up to that change.
pub(crate) fn bind(prepared: &mut Conversation, source: &Conversation) {
    let mut original = source.messages.clone();
    let mut routed = prepared.messages.clone();
    normalize_tool_boundaries(&mut original);
    normalize_tool_boundaries(&mut routed);
    let mut fingerprints = Vec::new();
    for (original, routed) in original.iter().zip(&routed) {
        let include_images = !routed.images().is_empty();
        if fingerprint(original, include_images) != fingerprint(routed, include_images) {
            break;
        }
        let Some(hash) = fingerprint(original, true) else {
            break;
        };
        fingerprints.push(hash);
    }
    prepared.bind_provider_request_source(source, fingerprints);
}

pub(crate) fn record(
    conversation: &Conversation,
    config: &ProviderConfig,
    model: &str,
    messages: &[ApiMessage],
    recovery_edit: Option<usize>,
    tools: &[Value],
) {
    let mut source_messages = if let Some(source) = conversation.provider_request_source() {
        source.to_vec()
    } else {
        let mut source = conversation.messages.clone();
        normalize_tool_boundaries(&mut source);
        source
            .iter()
            .map(|message| fingerprint(message, true))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    };
    let summary = conversation.summary.clone();
    let summary_count = usize::from(summary.baseline.is_some()) + summary.deltas.len();
    if let Some(edited_index) = recovery_edit {
        // The summary is serialized ahead of every user message.
        let Some(unchanged_transcript_messages) = edited_index.checked_sub(summary_count) else {
            return;
        };
        source_messages.truncate(unchanged_transcript_messages);
    }
    // The mapped transcript and summary end before the request's trailing
    // internal context and any other request-only suffix.
    let Some(messages) = messages.get(..source_messages.len() + summary_count) else {
        return;
    };
    let messages = messages
        .iter()
        .map(|message| {
            Some(CapturedProviderMessage {
                json: to_raw_value(message).ok()?,
                estimated_tokens: message.estimated_tokens(&DefaultTokenCounter),
                has_images: !message.images.is_empty(),
                has_tool_images: message.role == "tool" && !message.images.is_empty(),
            })
        })
        .collect::<Option<Vec<_>>>();
    if let Some(messages) = messages {
        conversation.record_provider_request(CapturedProviderRequest {
            source_messages,
            summary,
            endpoint: endpoint(config).to_string(),
            model: model.to_string(),
            reasoning_effort: config.reasoning_effort,
            messages,
            tools: tools.to_vec(),
        });
    }
}

pub(crate) struct ReplayPrefix {
    pub messages: Vec<Box<RawValue>>,
    pub estimated_tokens: usize,
    pub tools: Vec<Value>,
}

pub(crate) fn load(
    conversation: &Conversation,
    cut: usize,
    config: &ProviderConfig,
    model: &str,
) -> Option<ReplayPrefix> {
    let request = conversation.last_provider_request()?;
    if request.model != model
        || request.endpoint != endpoint(config)
        || request.reasoning_effort != config.reasoning_effort
        || request.summary != conversation.summary
        || (request.tools != deepseek_primary_request_tools(config)
            && request.tools != deepseek_plain_request_tools(config))
    {
        return None;
    }
    let source = request.source_messages.get(..cut)?;
    if !conversation
        .messages
        .get(..cut)?
        .iter()
        .zip(source)
        .all(|(message, hash)| fingerprint(message, true).as_ref() == Some(hash))
    {
        return None;
    }
    let summary_count =
        usize::from(request.summary.baseline.is_some()) + request.summary.deltas.len();
    let messages = request.messages.get(..cut + summary_count)?;
    // A screenshot added after the last request can change the next request's
    // image cap. Replay uses the earlier cap, image routing and tool variant
    // as sent. A resumed session or a cut beyond the mapped history falls back.
    if messages.iter().any(|message| {
        (message.has_images && model == PRO_MODEL)
            || (message.has_tool_images && !accepts_tool_images(model))
    }) {
        return None;
    }
    Some(ReplayPrefix {
        messages: messages
            .iter()
            .map(|message| message.json.clone())
            .collect(),
        estimated_tokens: messages
            .iter()
            .map(|message| message.estimated_tokens)
            .sum(),
        tools: request.tools.clone(),
    })
}
