use orca_core::config::ReasoningEffort;
use orca_core::conversation::Conversation;
use orca_provider::ProviderConfig;
use orca_provider::prompt_cache::checkpoint_for_deepseek_request;
use orca_provider::tool_schema::ProviderToolDefinition;
use serde_json::json;

fn config(tools: Vec<ProviderToolDefinition>) -> ProviderConfig {
    ProviderConfig {
        api_key: None,
        base_url: Some("https://api.deepseek.com/".to_string()),
        model: Some("deepseek-v4-flash".to_string()),
        reasoning_effort: ReasoningEffort::High,
        tools_override: Some(tools),
        mcp_registry: None,
        external_tools: Vec::new(),
    }
}

fn tool(name: &str) -> ProviderToolDefinition {
    ProviderToolDefinition {
        name: name.to_string(),
        description: format!("{name} tool"),
        input_schema: json!({
            "type": "object",
            "properties": { "value": { "type": "string" } }
        }),
        strict_capable: false,
    }
}

fn conversation(system: &str) -> Conversation {
    let mut conversation = Conversation::new();
    conversation.add_system(system.to_string());
    conversation.add_user("known private user prompt".to_string());
    conversation
}

#[test]
fn checkpoint_accepts_an_extended_lowered_message_prefix() {
    let base = conversation("known private system prompt");
    let mut extended = conversation("known private system prompt");
    extended.add_assistant(Some("acknowledged".to_string()), None, Vec::new());
    extended.add_user("new user request".to_string());
    let config = config(vec![tool("alpha")]);

    let checkpoint = checkpoint_for_deepseek_request(&base, &config).expect("checkpoint");

    assert!(
        checkpoint
            .matches_deepseek_prefix(&extended, &config)
            .expect("compare prefix")
    );
}

#[test]
fn checkpoint_rejects_a_changed_system_message() {
    let base = conversation("known private system prompt");
    let changed = conversation("changed system prompt");
    let config = config(vec![tool("alpha")]);
    let checkpoint = checkpoint_for_deepseek_request(&base, &config).expect("checkpoint");

    assert!(
        !checkpoint
            .matches_deepseek_prefix(&changed, &config)
            .expect("compare prefix")
    );
}

#[test]
fn checkpoint_rejects_changed_tools() {
    let base = conversation("known private system prompt");
    let mut extended = conversation("known private system prompt");
    extended.add_user("new user request".to_string());
    let checkpoint =
        checkpoint_for_deepseek_request(&base, &config(vec![tool("alpha")])).expect("checkpoint");

    assert!(
        !checkpoint
            .matches_deepseek_prefix(&extended, &config(vec![tool("zeta")]))
            .expect("compare prefix")
    );
}

#[test]
fn serialized_checkpoint_contains_no_known_prompt_text() {
    let checkpoint = checkpoint_for_deepseek_request(
        &conversation("known private system prompt"),
        &config(vec![tool("alpha")]),
    )
    .expect("checkpoint");
    let serialized = serde_json::to_string(&checkpoint).expect("serialize checkpoint");

    assert!(!serialized.contains("known private system prompt"));
    assert!(!serialized.contains("known private user prompt"));
    assert!(!serialized.contains("alpha tool"));
}
