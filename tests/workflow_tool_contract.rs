use orca_core::approval_types::ActionKind;
use orca_core::config::ProviderKind;
use orca_core::conversation::Conversation;
use orca_core::provider_types::ProviderStep;
use orca_core::tool_types::ToolName;
use orca_provider::{ProviderConfig, call};
use serde_json::Value;

#[test]
fn workflow_schema_is_registered_with_official_fields() {
    let registry = orca_tools::registry::default_tool_registry();
    let tool = registry.get("Workflow").expect("Workflow tool registered");
    let schema = &tool.spec().input_schema;
    let properties = &schema["properties"];

    assert_eq!(tool.name(), "Workflow");
    assert_eq!(tool.action_kind(), ActionKind::Agent);
    assert!(properties.get("script").is_some());
    assert!(properties.get("name").is_some());
    assert!(properties.get("description").is_some());
    assert!(properties.get("title").is_some());
    assert!(properties.get("args").is_some());
    assert!(properties.get("draftId").is_some());
    assert!(properties.get("scriptPath").is_some());
    assert!(properties.get("resumeFromRunId").is_some());
}

#[test]
fn workflow_draft_schema_is_registered_for_preview_creation() {
    let registry = orca_tools::registry::default_tool_registry();
    let tool = registry
        .get("WorkflowDraft")
        .expect("WorkflowDraft tool registered");
    let schema = &tool.spec().input_schema;
    let properties = &schema["properties"];

    assert_eq!(tool.name(), "WorkflowDraft");
    assert_eq!(tool.action_kind(), ActionKind::Write);
    let script_property = properties.get("script").expect("script property");
    assert!(
        script_property["description"]
            .as_str()
            .unwrap_or_default()
            .contains("preview")
    );
}

#[test]
fn workflow_draft_action_schema_is_registered_for_preview_decisions() {
    let registry = orca_tools::registry::default_tool_registry();
    let tool = registry
        .get("WorkflowDraftAction")
        .expect("WorkflowDraftAction tool registered");
    let schema = &tool.spec().input_schema;
    let properties = &schema["properties"];

    assert_eq!(tool.name(), "WorkflowDraftAction");
    assert_eq!(tool.action_kind(), ActionKind::Write);
    assert!(properties.get("draftId").is_some());
    assert!(properties.get("action").is_some());
    assert!(properties.get("saveAs").is_some());
    assert!(properties.get("scope").is_some());
    assert!(properties.get("script").is_some());
    assert!(
        properties["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "edit")
    );
}

#[test]
fn mock_provider_can_request_workflow_tool() {
    let mut conversation = Conversation::new();
    conversation.add_user("workflow inline".to_string());

    let response = call(
        ProviderKind::Mock,
        &conversation,
        &ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
            max_output_tokens: None,
        },
    );

    let tool_request = response
        .steps
        .iter()
        .find_map(|step| match step {
            ProviderStep::ToolCall(request) => Some(request),
            _ => None,
        })
        .expect("tool request");

    assert_eq!(tool_request.name, ToolName::Workflow);
    assert_eq!(tool_request.action, ActionKind::Agent);

    let raw_arguments = tool_request
        .raw_arguments
        .as_deref()
        .expect("raw arguments");
    let raw_arguments: Value = serde_json::from_str(raw_arguments).expect("valid raw arguments");
    let script = raw_arguments["script"].as_str().expect("workflow script");
    assert!(script.contains("export const meta"));
    assert!(script.contains("name: 'mock-workflow'"));
    assert!(script.contains("const result = await phase('main'"));
    assert!(script.contains("export default result;"));
    assert_eq!(raw_arguments["args"]["mode"], "inline");
}

#[test]
fn mock_provider_can_request_workflow_draft_tool() {
    let mut conversation = Conversation::new();
    conversation.add_user("workflow draft".to_string());

    let response = call(
        ProviderKind::Mock,
        &conversation,
        &ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: None,
            mcp_registry: None,
            external_tools: Vec::new(),
            max_output_tokens: None,
        },
    );

    let tool_request = response
        .steps
        .iter()
        .find_map(|step| match step {
            ProviderStep::ToolCall(request) => Some(request),
            _ => None,
        })
        .expect("tool request");

    assert_eq!(tool_request.name, ToolName::WorkflowDraft);
    assert_eq!(tool_request.action, ActionKind::Write);
    let raw_arguments = tool_request
        .raw_arguments
        .as_deref()
        .expect("raw arguments");
    let raw_arguments: Value = serde_json::from_str(raw_arguments).expect("valid raw arguments");
    assert_eq!(raw_arguments["script"], expected_workflow_script());
}

fn expected_workflow_script() -> &'static str {
    "export const meta = { name: 'mock-workflow', description: 'Mock workflow', phases: ['main'] };\nconst result = await phase('main', async () => agent('inspect repo'));\nexport default result;"
}

#[test]
fn workflow_tool_launches_background_task_and_returns_output() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "workflow inline",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);

    let completed = events
        .iter()
        .find(|event| {
            event["type"] == "tool.call.completed" && event["payload"]["name"] == "Workflow"
        })
        .expect("workflow tool completed");
    assert_eq!(completed["payload"]["status"], "completed");

    let output_text = completed["payload"]["output"].as_str().unwrap();
    let workflow_output: Value = serde_json::from_str(output_text).unwrap();
    assert_eq!(workflow_output["status"], "async_launched");
    assert_eq!(workflow_output["taskType"], "local_workflow");
    assert!(
        workflow_output["taskId"]
            .as_str()
            .unwrap()
            .starts_with("task-")
    );
    assert!(
        workflow_output["runId"]
            .as_str()
            .unwrap()
            .starts_with("workflow-run-")
    );

    let workflow_started = events
        .iter()
        .find(|event| event["type"] == "workflow.started")
        .expect("workflow started event");
    assert_eq!(workflow_started["payload"]["task"]["kind"], "workflow");
    assert_eq!(workflow_started["payload"]["task"]["status"], "running");
    let workflow_completed = events
        .iter()
        .find(|event| event["type"] == "workflow.completed")
        .expect("workflow completed event");
    assert_eq!(workflow_completed["payload"]["task"]["kind"], "workflow");
    assert_eq!(workflow_completed["payload"]["task"]["status"], "succeeded");
    let workflow_result_available = events
        .iter()
        .find(|event| event["type"] == "workflow.result.available")
        .expect("workflow result available event");
    assert_eq!(
        workflow_result_available["payload"]["task"]["status"],
        "succeeded"
    );
    let result_available_index = events
        .iter()
        .position(|event| event["type"] == "workflow.result.available")
        .expect("workflow result available event");
    let session_completed_index = events
        .iter()
        .position(|event| event["type"] == "session.completed")
        .expect("session completed event");
    assert!(
        result_available_index < session_completed_index,
        "workflow result should be emitted before session completion"
    );
}

#[test]
fn workflow_draft_tool_creates_preview_without_launching_workflow() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_orca"))
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "workflow draft",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);

    let completed = events
        .iter()
        .find(|event| {
            event["type"] == "tool.call.completed" && event["payload"]["name"] == "WorkflowDraft"
        })
        .expect("workflow draft tool completed");
    assert_eq!(completed["payload"]["status"], "completed");

    let output_text = completed["payload"]["output"].as_str().unwrap();
    let draft: Value = serde_json::from_str(output_text).unwrap();
    assert!(
        draft["draftId"]
            .as_str()
            .unwrap()
            .starts_with("workflow-draft-")
    );
    assert_eq!(draft["name"], "mock-workflow");
    assert_eq!(draft["description"], "Mock workflow");
    assert_eq!(draft["phases"], serde_json::json!(["main"]));
    assert_eq!(draft["estimatedAgentCount"], 1);
    assert_eq!(draft["sourceMutationRisk"], "read_only_likely");
    assert!(draft["scriptPath"].as_str().unwrap().ends_with("script.js"));

    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "workflow.started"),
        "draft preview must not launch the workflow"
    );
}

#[test]
fn workflow_draft_action_save_persists_preview_as_project_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_orca"))
        .current_dir(temp.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--approval-mode",
            "full-auto",
            "workflow draft action save",
        ])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));
    let events = parse_jsonl(&output.stdout);

    assert!(events.iter().any(|event| {
        event["type"] == "tool.call.completed" && event["payload"]["name"] == "WorkflowDraft"
    }));
    let action_completed = events
        .iter()
        .find(|event| {
            event["type"] == "tool.call.completed"
                && event["payload"]["name"] == "WorkflowDraftAction"
        })
        .expect("workflow draft action completed");
    let output_text = action_completed["payload"]["output"].as_str().unwrap();
    let action_output: Value = serde_json::from_str(output_text).unwrap();
    assert_eq!(action_output["status"], "saved");
    assert_eq!(action_output["action"], "save");
    assert_eq!(action_output["workflowName"], "mock-workflow");

    let saved = temp
        .path()
        .join(".orca")
        .join("workflows")
        .join("mock-workflow.js");
    assert!(saved.exists(), "saved project workflow should exist");
    assert_eq!(
        std::fs::read_to_string(saved).unwrap(),
        expected_workflow_script()
    );
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "workflow.started"),
        "saving a draft must not launch the workflow"
    );
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}
