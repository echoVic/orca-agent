//! Credential-gated long-context compaction smoke against the real DeepSeek API.
//!
//! Run with `cargo run -p orca-provider --example compaction_realapi --locked`.
//! The fixture is intentionally small enough for a repeatable smoke, but large
//! enough to cross the configured soft line and require a remote summary.

use std::collections::HashMap;

use orca_core::config::{ProviderKind, ReasoningEffort};
use orca_core::conversation::{Conversation, Message};
use orca_provider::ProviderConfig;
use orca_provider::context::{self, CompactionKind, ContextConfig};

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
            "DEEPSEEK_API_KEY not found (env or ~/.orca/auth.json); skipping compaction smoke."
        );
        return;
    };

    let provider_config = ProviderConfig {
        api_key: Some(api_key),
        base_url: None,
        model: Some("deepseek-v4-flash".to_string()),
        reasoning_effort: ReasoningEffort::Max,
        tools_override: Some(Vec::new()),
        mcp_registry: None,
        external_tools: Vec::new(),
    };
    let context_config = ContextConfig {
        max_tokens: 6_000,
        compaction_threshold: 1.0,
        reserved_for_response: 0,
        auto_compact_token_limit: None,
        soft_compact_token_limit: Some(2_500),
    };

    let mut conversation = Conversation::new();
    conversation.add_system(
        "You are evaluating a long-context compaction boundary. Preserve exact facts.".to_string(),
    );
    for index in 0..16 {
        conversation.add_user(format!(
            "Historical request {index}: retain the file path src/module_{index}.rs, the decision number {index}, and the blocker marker BLOCKER-{index}. {}",
            "stable historical context ".repeat(42)
        ));
        conversation.add_assistant(
            Some(format!(
                "Historical answer {index}: acknowledged decision {index} and blocker BLOCKER-{index}. {}",
                "stable answer context ".repeat(42)
            )),
            None,
            Vec::new(),
        );
    }
    conversation.add_user("Current request: report the retained compaction boundary.".to_string());

    let before_messages = conversation.messages.len();
    let before_pressure =
        context::context_pressure(&conversation, &context_config, &provider_config);
    assert!(
        before_pressure.should_soft_compact,
        "fixture did not cross the configured soft compaction line: {before_pressure:?}"
    );

    let result = context::compact_with_summary(
        ProviderKind::DeepSeek,
        &conversation,
        &context_config,
        &provider_config,
    );
    let CompactionKind::RemoteSummary(summary) = result.kind else {
        eprintln!("remote summary was unavailable; compaction fell back to local truncation");
        std::process::exit(1);
    };
    assert!(
        !summary.trim().is_empty(),
        "remote summary must contain text"
    );
    assert!(result.conversation.summary.baseline.is_some());
    assert!(result.conversation.messages.iter().any(|message| {
        matches!(message, Message::User { content, .. } if content.contains("Current request"))
    }));

    let after_pressure =
        context::context_pressure(&result.conversation, &context_config, &provider_config);
    assert!(
        after_pressure.wire_tokens < before_pressure.wire_tokens,
        "compaction must reduce wire pressure: before={before_pressure:?} after={after_pressure:?}"
    );
    println!(
        "remote compaction verified: messages {before_messages}->{}; wire_tokens {}->{}; summary_chars={}",
        result.conversation.messages.len(),
        before_pressure.wire_tokens,
        after_pressure.wire_tokens,
        summary.len()
    );
}
