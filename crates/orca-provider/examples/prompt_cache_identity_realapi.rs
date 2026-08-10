//! Credential-gated DeepSeek prompt-cache identity verifier.
//!
//! Run with `cargo run -p orca-provider --example prompt_cache_identity_realapi`.
//! The verifier never prints the API key. Without a configured credential it
//! reports a successful skip so local CI can run the example safely.

use std::collections::HashMap;

use orca_core::config::ProviderKind;
use orca_core::conversation::{Conversation, Message};
use orca_provider::{ProviderConfig, call};

fn load_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    let path = dirs::home_dir()?.join(".orca").join("auth.json");
    let content = std::fs::read_to_string(path).ok()?;
    let values: HashMap<String, String> = serde_json::from_str(&content).ok()?;
    values
        .get("DEEPSEEK_API_KEY")
        .filter(|key| !key.is_empty())
        .cloned()
}

fn main() {
    let Some(api_key) = load_api_key() else {
        eprintln!(
            "DEEPSEEK_API_KEY not found (env or ~/.orca/auth.json); skipping prompt-cache identity verifier."
        );
        return;
    };

    let config = ProviderConfig {
        api_key: Some(api_key),
        base_url: None,
        model: Some("deepseek-v4-flash".to_string()),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        tools_override: Some(Vec::new()),
        mcp_registry: None,
        external_tools: Vec::new(),
    };

    let mut conversation = Conversation::new();
    conversation.add_system(format!(
        "You are verifying a deterministic shared prompt prefix. {}",
        (0..64)
            .map(|index| {
                format!(
                    "Stable prefix segment {index}: preserve exact ordering and punctuation for cache identity."
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    ));
    conversation.add_user(
        "Acknowledge the stable prefix in one short sentence, without quoting it.".to_string(),
    );

    let first = call(ProviderKind::DeepSeek, &conversation, &config);
    let first_usage = first.usage.clone().unwrap_or_default();
    let Some(first_content) = first.assistant_content.clone() else {
        eprintln!("first DeepSeek request did not return assistant content: {first:?}");
        std::process::exit(1);
    };
    conversation.messages.push(Message::Assistant {
        content: Some(first_content),
        reasoning_content: first.assistant_reasoning.clone(),
        tool_calls: Vec::new(),
        pinned: false,
    });
    conversation.add_user("Now answer only: cache verification complete.".to_string());

    let second = call(ProviderKind::DeepSeek, &conversation, &config);
    let second_usage = second.usage.clone().unwrap_or_default();
    println!(
        "first prompt_tokens={} cache_tokens={}; second prompt_tokens={} cache_tokens={}",
        first_usage.input_tokens,
        first_usage.cache_tokens,
        second_usage.input_tokens,
        second_usage.cache_tokens
    );
    if second_usage.cache_tokens == 0 {
        eprintln!("DeepSeek prompt-cache verifier failed: second usage.cache_tokens was zero");
        std::process::exit(1);
    }
}
