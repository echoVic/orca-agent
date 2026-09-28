use std::process::Command;

use orca_core::approval_types::ActionKind;
use orca_core::external_config::ExternalToolConfig;
use orca_core::mcp_types::McpTool;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_provider::tool_schema::{
    ProviderToolDefinition, deepseek_strict_tools_schema_for_endpoint, deepseek_tools_schema,
};
use orca_tools::schema::{ToolPolicy, canonical_tool_definitions};
use serde_json::Value;

#[test]
fn tool_schema_preserves_canonical_definitions_across_agent_policies() {
    let mcp = McpRegistry::from_tools_for_test(vec![McpTool {
        server: "local".to_string(),
        name: "inspect".to_string(),
        schema_name: "mcp__local__inspect".to_string(),
        description: Some("Inspect with MCP".to_string()),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        }),
        read_only: false,
    }]);
    let external = ExternalToolConfig {
        name: "external_lookup".to_string(),
        description: "External lookup".to_string(),
        action_kind: ActionKind::Read,
        command: "true".to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"],
            "additionalProperties": false
        }),
    };
    let registry = orca_tools::registry::tool_registry_with_mcp_and_external(
        Some(&mcp),
        std::slice::from_ref(&external),
    );

    let root = canonical_tool_definitions(&ToolPolicy::base(), &registry);
    let read_file = definition(&root, "read_file");
    assert!(read_file.description.contains("Read the contents"));
    assert_eq!(
        read_file.input_schema["required"],
        serde_json::json!(["path"])
    );
    assert!(!read_file.strict_capable);
    assert!(definition(&root, "update_plan").strict_capable);
    assert!(!root.iter().any(|tool| tool.name == "update_goal"));

    let goal = canonical_tool_definitions(&ToolPolicy::goal(), &registry);
    assert!(goal.iter().any(|tool| tool.name == "get_goal"));
    assert!(goal.iter().any(|tool| tool.name == "create_goal"));
    assert!(goal.iter().any(|tool| tool.name == "update_goal"));

    let child = canonical_tool_definitions(
        &ToolPolicy::for_subagent(&SubagentType::CodeReviewer),
        &registry,
    );
    assert!(child.iter().any(|tool| tool.name == "read_file"));
    assert!(child.iter().any(|tool| tool.name == "glob"));
    assert!(!child.iter().any(|tool| tool.name == "subagent"));
    assert!(!child.iter().any(|tool| tool.name == "mcp__local__inspect"));
    assert!(!child.iter().any(|tool| tool.name == "external_lookup"));
}

#[test]
fn tool_schema_lowering_is_generic_and_preserves_deepseek_wire_shape() {
    let definitions = vec![ProviderToolDefinition {
        name: "arbitrary_tool".to_string(),
        description: "Arbitrary provider-neutral tool".to_string(),
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
    }];

    let wire = deepseek_tools_schema(&definitions);
    assert_eq!(wire[0]["type"], "function");
    assert_eq!(wire[0]["function"]["name"], "arbitrary_tool");
    assert_eq!(
        wire[0]["function"]["description"],
        "Arbitrary provider-neutral tool"
    );
    assert_eq!(
        wire[0]["function"]["parameters"],
        definitions[0].input_schema
    );
    assert!(wire[0]["function"].get("strict").is_none());

    let strict =
        deepseek_strict_tools_schema_for_endpoint(&definitions, "https://api.deepseek.com/beta")
            .expect("strict beta schema");
    assert_eq!(strict[0]["function"]["strict"], true);
    assert_eq!(
        strict[0]["function"]["parameters"]["required"],
        serde_json::json!(["optional_value", "required_value"])
    );
}

fn definition<'a>(
    definitions: &'a [orca_tools::schema::CanonicalToolDefinition],
    name: &str,
) -> &'a orca_tools::schema::CanonicalToolDefinition {
    definitions
        .iter()
        .find(|definition| definition.name == name)
        .unwrap_or_else(|| panic!("missing canonical definition {name}"))
}

#[test]
fn deepseek_fixture_preserves_reasoning_and_replay_state() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "deepseek-fixture",
            "inspect repo",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let events = parse_jsonl(&output.stdout);
    assert_eq!(events[0]["payload"]["provider"], "deepseek-fixture");

    let reasoning = find_event(&events, "assistant.reasoning.delta");
    assert!(
        reasoning["payload"]["text"]
            .as_str()
            .unwrap()
            .contains("DeepSeek fixture reasoning")
    );

    let replay = find_event(&events, "provider.replay.updated");
    assert_eq!(replay["payload"]["provider"], "deepseek");
    assert!(
        replay["payload"]["reasoning_content"]
            .as_str()
            .unwrap()
            .contains("DeepSeek fixture reasoning")
    );
    assert_eq!(replay["payload"]["tool_call_ids"][0], "fixture-tool-1");

    let tool = find_event(&events, "tool.call.requested");
    assert_eq!(tool["payload"]["id"], "fixture-tool-1");
    assert_eq!(tool["payload"]["name"], "read_file");

    assert!(!events.iter().any(|event| {
        event["type"] == "assistant.message.delta"
            && event["payload"]["text"]
                .as_str()
                .unwrap_or("")
                .contains("Mock runtime completed one tool request")
    }));

    assert_eq!(events.last().unwrap()["payload"]["status"], "success");
}

#[test]
fn deepseek_provider_without_api_key_emits_error_and_fails() {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env_remove("DEEPSEEK_API_KEY")
        .env("HOME", "/tmp/orca_test_no_home")
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "deepseek",
            "inspect repo",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(1));

    let events = parse_jsonl(&output.stdout);
    let error = find_event(&events, "error");
    assert!(
        error["payload"]["message"]
            .as_str()
            .unwrap()
            .contains("DEEPSEEK_API_KEY")
    );
    assert_eq!(events.last().unwrap()["payload"]["status"], "failed");
}

fn find_event<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"] == event_type)
        .unwrap_or_else(|| panic!("missing {event_type}"))
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
