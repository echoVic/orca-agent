use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use orca_core::conversation::Conversation;

use crate::ProviderConfig;
use crate::deepseek_http::{
    DEFAULT_BASE_URL, DEFAULT_MODEL, conversation_to_api_messages, deepseek_primary_request_tools,
};

const PROMPT_CACHE_CHECKPOINT_VERSION: u8 = 1;
const SCOPE_DOMAIN: &[u8] = b"orca.prompt-cache.scope.v1\0";
const MESSAGES_DOMAIN: &[u8] = b"orca.prompt-cache.messages.v1\0";
const TOOLS_DOMAIN: &[u8] = b"orca.prompt-cache.tools.v1\0";

/// Content-free DeepSeek prompt-cache identity recorded beside a provider turn.
/// It is audit metadata only and never reconstructs conversation state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromptCacheCheckpoint {
    pub version: u8,
    pub scope_sha256: String,
    pub message_prefix_sha256: String,
    pub message_count: u32,
    pub tool_schema_sha256: String,
    pub tool_count: u32,
}

impl PromptCacheCheckpoint {
    /// Checks whether a later request keeps this checkpoint's exact lowered
    /// message prefix and its primary preflight scope and tool payload.
    pub fn matches_deepseek_prefix(
        &self,
        conversation: &Conversation,
        config: &ProviderConfig,
    ) -> serde_json::Result<bool> {
        if self.version != PROMPT_CACHE_CHECKPOINT_VERSION {
            return Ok(false);
        }

        let scope_sha256 = scope_sha256(config)?;
        let tools = deepseek_primary_request_tools(config);
        if self.scope_sha256 != scope_sha256
            || self.tool_schema_sha256 != hash_json(TOOLS_DOMAIN, &tools)?
            || self.tool_count != tools.len() as u32
        {
            return Ok(false);
        }

        let messages = conversation_to_api_messages(conversation);
        let Some(prefix) = messages.get(..self.message_count as usize) else {
            return Ok(false);
        };
        Ok(self.message_prefix_sha256 == hash_json(MESSAGES_DOMAIN, prefix)?)
    }
}

/// Creates a content-free cache checkpoint from the exact conversation lowering
/// and primary preflight tool payload sent to DeepSeek.
pub fn checkpoint_for_deepseek_request(
    conversation: &Conversation,
    config: &ProviderConfig,
) -> serde_json::Result<PromptCacheCheckpoint> {
    let messages = conversation_to_api_messages(conversation);
    let tools = deepseek_primary_request_tools(config);
    Ok(PromptCacheCheckpoint {
        version: PROMPT_CACHE_CHECKPOINT_VERSION,
        scope_sha256: scope_sha256(config)?,
        message_prefix_sha256: hash_json(MESSAGES_DOMAIN, &messages)?,
        message_count: messages.len() as u32,
        tool_schema_sha256: hash_json(TOOLS_DOMAIN, &tools)?,
        tool_count: tools.len() as u32,
    })
}

fn scope_sha256(config: &ProviderConfig) -> serde_json::Result<String> {
    let endpoint = config
        .base_url
        .as_deref()
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/');
    let model = config.model.as_deref().unwrap_or(DEFAULT_MODEL);
    hash_json(
        SCOPE_DOMAIN,
        &json!({
            "endpoint": endpoint,
            "model": model,
            "reasoning_effort": config.reasoning_effort.as_str(),
        }),
    )
}

fn hash_json<T: Serialize + ?Sized>(domain: &[u8], value: &T) -> serde_json::Result<String> {
    let bytes = serde_json::to_vec(value)?;
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}
