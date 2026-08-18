use std::collections::HashMap;

use orca_core::proposed_plan::{ProposedPlanSegment, ProposedPlanStreamParser};
use orca_core::thread_item_projection::{CompletedModelItem, CompletedModelResponse};
use serde_json::{Value, json};

use crate::protocol::{self, ServerEvent};
use crate::tool_item_projection::{
    ProjectedFileChangeItem, ProjectedToolCallCompletion, ProjectedToolCallItem,
    ProjectedWorkflowItem, mcp_result_from_content, mcp_tool_parts, parse_json_or_null,
    tool_error_object_from_value, tool_status_is_completed, unified_exec_projection,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct RuntimeEventProjector {
    agent_message_id: Option<String>,
    plan_item_id: Option<String>,
    plan_parser: ProposedPlanStreamParser,
    reasoning_item_id: Option<String>,
    tool_items: HashMap<String, ProjectedToolCallItem>,
    file_change_items: HashMap<String, ProjectedFileChangeItem>,
    workflow_items: HashMap<String, ProjectedWorkflowItem>,
}

impl RuntimeEventProjector {
    pub(crate) fn project_line(&mut self, line: &str) -> Vec<ServerEvent> {
        let runtime_event: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        let event_type = runtime_event["type"].as_str().unwrap_or_default();
        let mut events = Vec::new();

        match event_type {
            "assistant.message.delta" => {
                self.project_assistant_message_delta(&runtime_event, &mut events);
            }
            "assistant.reasoning.delta" => {
                self.project_reasoning_delta(&runtime_event, &mut events);
            }
            "model.response.completed" => {
                self.project_completed_model_response(&runtime_event, &mut events);
            }
            "tool.call.requested" => {
                self.project_tool_item_started(&runtime_event, &mut events);
                self.project_file_change_item_started(&runtime_event, &mut events);
            }
            "workflow.started" => {
                self.project_workflow_item_started(&runtime_event, &mut events);
            }
            _ => {}
        }

        if let Some(event) = protocol::map_runtime_event_line(line) {
            events.push(event);
        }

        match event_type {
            "tool.call.completed" => {
                self.project_tool_item_completed(&runtime_event, &mut events);
                self.project_file_change_item_completed(&runtime_event, &mut events);
            }
            "workflow.completed" => {
                self.record_workflow_completed(&runtime_event);
            }
            "workflow.result.available" => {
                self.record_workflow_result(&runtime_event);
                self.project_workflow_item_completed(&runtime_event, "completed", &mut events);
            }
            "workflow.failed" => {
                self.project_workflow_item_completed(&runtime_event, "failed", &mut events);
            }
            "session.completed" => {
                self.clear_transient_text_items();
            }
            _ => {}
        }

        events
    }

    fn project_reasoning_delta(&mut self, runtime_event: &Value, events: &mut Vec<ServerEvent>) {
        let payload = &runtime_event["payload"];
        let Some(item_id) = payload["item_id"].as_str() else {
            return;
        };
        let delta = payload["text"].as_str().unwrap_or_default();
        if self.reasoning_item_id.as_deref() != Some(item_id) {
            self.reasoning_item_id = Some(item_id.to_string());
            events.push(ServerEvent::ItemStarted {
                thread_id: runtime_event["run_id"].clone(),
                turn_id: payload["turn_id"].clone(),
                item: json!({
                    "type": "reasoning",
                    "id": item_id,
                    "summary": "",
                    "content": "",
                }),
            });
        }
        events.push(ServerEvent::ItemReasoningDelta {
            item_id: Value::from(item_id.to_string()),
            delta: Value::from(delta.to_string()),
        });
    }

    fn project_assistant_message_delta(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let Some(agent_message_item_id) = payload["agent_message_item_id"].as_str() else {
            return;
        };
        let Some(plan_item_id) = payload["plan_item_id"].as_str() else {
            return;
        };
        let delta = payload["text"].as_str().unwrap_or_default();
        for segment in self.plan_parser.push(delta) {
            match segment {
                ProposedPlanSegment::Agent(text) => self.project_agent_message_delta(
                    runtime_event,
                    agent_message_item_id,
                    &text,
                    events,
                ),
                ProposedPlanSegment::Plan(text) => {
                    self.project_plan_delta(runtime_event, plan_item_id, &text, events)
                }
            }
        }
    }

    fn flush_assistant_message_parser(
        &mut self,
        runtime_event: &Value,
        agent_message_item_id: &str,
        plan_item_id: &str,
        events: &mut Vec<ServerEvent>,
    ) {
        for segment in self.plan_parser.finish() {
            match segment {
                ProposedPlanSegment::Agent(text) => self.project_agent_message_delta(
                    runtime_event,
                    agent_message_item_id,
                    &text,
                    events,
                ),
                ProposedPlanSegment::Plan(text) => {
                    self.project_plan_delta(runtime_event, plan_item_id, &text, events)
                }
            }
        }
    }

    fn project_agent_message_delta(
        &mut self,
        runtime_event: &Value,
        item_id: &str,
        delta: &str,
        events: &mut Vec<ServerEvent>,
    ) {
        if delta.is_empty() {
            return;
        }
        if self.agent_message_id.as_deref() != Some(item_id) {
            self.agent_message_id = Some(item_id.to_string());
            events.push(ServerEvent::ItemStarted {
                thread_id: runtime_event["run_id"].clone(),
                turn_id: runtime_event["payload"]["turn_id"].clone(),
                item: json!({
                    "type": "agent_message",
                    "id": item_id,
                    "text": "",
                }),
            });
        }
        events.push(ServerEvent::ItemMessageDelta {
            item_id: Value::from(item_id.to_string()),
            delta: Value::from(delta.to_string()),
        });
    }

    fn project_plan_delta(
        &mut self,
        runtime_event: &Value,
        item_id: &str,
        delta: &str,
        events: &mut Vec<ServerEvent>,
    ) {
        if delta.is_empty() {
            return;
        }
        if self.plan_item_id.as_deref() != Some(item_id) {
            self.plan_item_id = Some(item_id.to_string());
            events.push(ServerEvent::ItemStarted {
                thread_id: runtime_event["run_id"].clone(),
                turn_id: runtime_event["payload"]["turn_id"].clone(),
                item: json!({
                    "type": "plan",
                    "id": item_id,
                    "text": "",
                }),
            });
        }
        events.push(ServerEvent::ItemPlanDelta {
            item_id: Value::from(item_id.to_string()),
            delta: Value::from(delta.to_string()),
        });
    }

    fn project_completed_model_response(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let Ok(response) =
            serde_json::from_value::<CompletedModelResponse>(runtime_event["payload"].clone())
        else {
            return;
        };
        self.flush_assistant_message_parser(
            runtime_event,
            response.identity.item_ids.agent_message_item_id().as_str(),
            response.identity.item_ids.plan_item_id.as_str(),
            events,
        );
        for item in response.completed_items() {
            self.ensure_completed_item_started(runtime_event, &item, events);
            events.push(ServerEvent::ItemCompleted {
                thread_id: runtime_event["run_id"].clone(),
                turn_id: Value::from(response.turn_id().to_string()),
                item: item.into_value(),
            });
        }
        self.clear_transient_text_items();
    }

    fn ensure_completed_item_started(
        &mut self,
        runtime_event: &Value,
        item: &CompletedModelItem,
        events: &mut Vec<ServerEvent>,
    ) {
        let already_started = match item {
            CompletedModelItem::AgentMessage { id, .. } => {
                self.agent_message_id.as_deref() == Some(id.as_str())
            }
            CompletedModelItem::Plan { id, .. } => {
                self.plan_item_id.as_deref() == Some(id.as_str())
            }
            CompletedModelItem::Reasoning { id, .. } => {
                self.reasoning_item_id.as_deref() == Some(id.as_str())
            }
        };
        if !already_started {
            events.push(ServerEvent::ItemStarted {
                thread_id: runtime_event["run_id"].clone(),
                turn_id: runtime_event["payload"]["identity"]["turn_id"].clone(),
                item: item.started_item().into_value(),
            });
        }
    }

    fn clear_transient_text_items(&mut self) {
        self.agent_message_id = None;
        self.plan_item_id = None;
        self.reasoning_item_id = None;
        self.plan_parser = ProposedPlanStreamParser::default();
    }

    fn project_tool_item_started(&mut self, runtime_event: &Value, events: &mut Vec<ServerEvent>) {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let tool = payload["name"].as_str().unwrap_or_default().to_string();
        if let Some((server, local_tool)) = mcp_tool_parts(&tool) {
            let item = ProjectedToolCallItem::mcp_tool(tool_id.clone(), server, local_tool);
            let started_item = item.started_item(tool_arguments(payload));
            self.tool_items.insert(tool_id.clone(), item);
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: started_item,
            });
            return;
        }
        if is_dynamic_tool(&tool) {
            let item = ProjectedToolCallItem::dynamic_tool(tool_id.clone(), tool);
            let started_item = item.started_item(tool_arguments(payload));
            self.tool_items.insert(tool_id.clone(), item);
            events.push(ServerEvent::ItemStarted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: started_item,
            });
            return;
        }
        let command = payload["target"].as_str().map(ToString::to_string);
        let item = ProjectedToolCallItem::command_execution(tool_id.clone(), tool, command.clone());
        let started_item = item.started_item(Value::Null);
        self.tool_items.insert(tool_id.clone(), item);
        events.push(ServerEvent::ItemStarted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: started_item,
        });
    }

    fn project_tool_item_completed(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let item = self.tool_items.remove(&tool_id).unwrap_or_else(|| {
            fallback_projected_tool_call_item(
                tool_id,
                payload["name"].as_str().unwrap_or_default(),
                payload["target"].as_str(),
            )
        });
        let unified_exec = payload["output"].as_str().and_then(|content| {
            unified_exec_projection(payload["name"].as_str().unwrap_or_default(), content)
        });
        events.push(ServerEvent::ItemCompleted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: item.completed_item(ProjectedToolCallCompletion {
                status: payload["status"].as_str().unwrap_or_default().to_string(),
                command_status: unified_exec
                    .as_ref()
                    .map(|projection| projection.status.clone())
                    .unwrap_or_else(|| payload["status"].clone()),
                arguments: tool_arguments(payload),
                result: mcp_tool_result(payload),
                command_error: unified_exec
                    .as_ref()
                    .map(|projection| projection.error.clone())
                    .unwrap_or_else(|| payload["error"].clone()),
                mcp_error: mcp_tool_error(payload),
                dynamic_error: dynamic_tool_error(payload),
                content_items: dynamic_tool_content_items(payload),
                success: payload["status"].as_str() == Some("completed"),
                aggregated_output: unified_exec
                    .as_ref()
                    .map(|projection| projection.output.clone())
                    .unwrap_or_else(|| payload["output"].clone()),
                exit_code: unified_exec
                    .as_ref()
                    .map(|projection| projection.exit_code.clone())
                    .unwrap_or_else(|| payload["exit_code"].clone()),
                truncated: unified_exec
                    .as_ref()
                    .map(|projection| projection.truncated.clone())
                    .unwrap_or_else(|| payload["truncated"].clone()),
            }),
        });
    }

    fn project_file_change_item_started(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let tool = payload["name"].as_str().unwrap_or_default().to_string();
        let Some(kind) = file_change_kind(&tool) else {
            return;
        };
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let target = payload["target"].as_str();
        let path = file_change_path(&tool, target);
        let item = ProjectedFileChangeItem::new(format!("{tool_id}:file-change"), path, kind);
        let started_item = item.started_item(file_change_diff());
        self.file_change_items.insert(tool_id, item);
        events.push(ServerEvent::ItemStarted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: started_item,
        });
    }

    fn project_file_change_item_completed(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let tool_id = payload["id"].as_str().unwrap_or("tool-call").to_string();
        let Some(item) = self.file_change_items.remove(&tool_id) else {
            return;
        };
        events.push(ServerEvent::ItemCompleted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: item.completed_item(file_change_status(payload), file_change_diff()),
        });
    }

    fn project_workflow_item_started(
        &mut self,
        runtime_event: &Value,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"]
            .as_str()
            .unwrap_or("workflow-run")
            .to_string();
        let task_id = payload["taskId"].as_str().unwrap_or_default().to_string();
        let workflow_name = payload["workflowName"]
            .as_str()
            .unwrap_or("workflow")
            .to_string();
        let item = ProjectedWorkflowItem::started(
            run_id.clone(),
            task_id,
            workflow_name,
            payload["task"].clone(),
        );
        let started_item = item.started_item();
        self.workflow_items.insert(run_id, item);
        events.push(ServerEvent::ItemStarted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: started_item,
        });
    }

    fn record_workflow_result(&mut self, runtime_event: &Value) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"].as_str().unwrap_or("workflow-run");
        if let Some(item) = self.workflow_items.get_mut(run_id) {
            item.record_result(payload["result"].clone(), payload["task"].clone());
        }
    }

    fn record_workflow_completed(&mut self, runtime_event: &Value) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"].as_str().unwrap_or("workflow-run");
        if let Some(item) = self.workflow_items.get_mut(run_id) {
            item.record_completed(payload["task"].clone());
        }
    }

    fn project_workflow_item_completed(
        &mut self,
        runtime_event: &Value,
        status: &str,
        events: &mut Vec<ServerEvent>,
    ) {
        let payload = &runtime_event["payload"];
        let run_id = payload["runId"]
            .as_str()
            .unwrap_or("workflow-run")
            .to_string();
        let fallback = ProjectedWorkflowItem::started(
            run_id.clone(),
            payload["taskId"].as_str().unwrap_or_default(),
            payload["workflowName"].as_str().unwrap_or("workflow"),
            payload["task"].clone(),
        );
        let mut item = self.workflow_items.remove(&run_id).unwrap_or(fallback);
        item.fill_task_if_missing(payload["task"].clone());
        events.push(ServerEvent::ItemCompleted {
            thread_id: Value::Null,
            turn_id: Value::Null,
            item: item.completed_item(status, payload["error"].clone()),
        });
    }
}

fn file_change_kind(tool: &str) -> Option<&'static str> {
    match tool {
        "edit" => Some("edit"),
        "write_file" => Some("write"),
        _ => None,
    }
}

fn file_change_path(tool: &str, target: Option<&str>) -> Option<String> {
    let target = target?.trim();
    if target.is_empty() {
        return None;
    }
    match tool {
        "edit" => Some(
            target
                .split_once("::")
                .map(|(path, _)| path)
                .unwrap_or(target)
                .trim()
                .to_string(),
        ),
        "write_file" => Some(target.to_string()),
        _ => None,
    }
}

fn file_change_status(payload: &Value) -> Value {
    match payload["status"].as_str() {
        Some("in_progress") => Value::from("inProgress"),
        Some(status) => Value::from(status.to_string()),
        None => Value::Null,
    }
}

fn file_change_diff() -> Value {
    Value::from(String::new())
}

fn is_dynamic_tool(tool: &str) -> bool {
    !is_builtin_tool(tool) && mcp_tool_parts(tool).is_none()
}

fn fallback_projected_tool_call_item(
    id: String,
    tool: &str,
    target: Option<&str>,
) -> ProjectedToolCallItem {
    if let Some((server, local_tool)) = mcp_tool_parts(tool) {
        return ProjectedToolCallItem::mcp_tool(id, server, local_tool);
    }
    if is_dynamic_tool(tool) {
        return ProjectedToolCallItem::dynamic_tool(id, tool.to_string());
    }
    ProjectedToolCallItem::command_execution(id, tool.to_string(), target.map(ToString::to_string))
}

fn is_builtin_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read_file"
            | "list_files"
            | "glob"
            | "grep"
            | "bash"
            | "exec_command"
            | "write_stdin"
            | "edit"
            | "write_file"
            | "git_status"
            | "subagent"
            | "subagent_status"
            | "task_list"
            | "task_stop"
            | "WorkflowDraft"
            | "workflow_draft"
            | "WorkflowDraftAction"
            | "workflow_draft_action"
            | "Workflow"
            | "workflow"
            | "workflow_send_message"
            | "workflow_read_messages"
            | "workflow_clear_messages"
            | "workflow_create_task_list"
            | "workflow_claim_task"
            | "workflow_complete_task"
            | "workflow_list_tasks"
            | "web_search"
            | "get_goal"
            | "create_goal"
            | "update_goal"
            | "update_plan"
            | "request_user_input"
            | "list_skills"
            | "read_skill"
    )
}

fn tool_arguments(payload: &Value) -> Value {
    payload["raw_arguments"]
        .as_str()
        .or_else(|| payload["target"].as_str())
        .map(parse_json_or_null)
        .unwrap_or(Value::Null)
}

fn mcp_tool_result(payload: &Value) -> Value {
    if !tool_status_is_completed(payload) || !payload["error"].is_null() {
        return Value::Null;
    }
    let Some(output) = payload["output"].as_str() else {
        return Value::Null;
    };
    mcp_result_from_content(output)
}

fn mcp_tool_error(payload: &Value) -> Value {
    if let Some(error) = payload["error"].as_str() {
        return tool_error_object_from_value(error, payload);
    }
    if payload["status"].as_str() == Some("failed") {
        if let Some(output) = payload["output"].as_str() {
            return tool_error_object_from_value(output, payload);
        }
        return tool_error_object_from_value("MCP tool call failed", payload);
    }
    Value::Null
}

fn dynamic_tool_content_items(payload: &Value) -> Value {
    if !tool_status_is_completed(payload) {
        return Value::Null;
    }
    match payload["output"].as_str() {
        Some(output) => json!([{ "type": "text", "text": output }]),
        None => Value::Null,
    }
}

fn dynamic_tool_error(payload: &Value) -> Value {
    match tool_error_detail(payload) {
        Value::String(message) => tool_error_object_from_value(&message, payload),
        Value::Null => Value::Null,
        other => other,
    }
}

fn tool_error_detail(payload: &Value) -> Value {
    if let Some(error) = payload["error"].as_str() {
        return Value::from(error.to_string());
    }
    if !tool_status_is_completed(payload) {
        if let Some(output) = payload["output"].as_str() {
            return Value::from(output.to_string());
        }
        return Value::from("tool call failed");
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::event_schema::EventFactory;
    use orca_core::event_sink::EventSink;
    use orca_core::thread_identity::TurnId;
    use orca_core::thread_item_projection::{CompletedModelResponse, ModelResponseIdentity};
    use orca_core::{config::OutputFormat, event_schema::EventDraft};

    fn runtime_line(event: EventDraft) -> String {
        let mut output = Vec::new();
        EventSink::new(&mut output, OutputFormat::Jsonl)
            .emit(event)
            .expect("serialize runtime event");
        String::from_utf8(output)
            .expect("runtime event is utf8")
            .trim()
            .to_string()
    }

    #[test]
    fn consecutive_model_responses_keep_distinct_item_lifecycles() {
        let turn_id = TurnId::new();
        let first_identity = ModelResponseIdentity::new(turn_id.clone());
        let second_identity = ModelResponseIdentity::new(turn_id);
        let mut factory = EventFactory::new("thread-model-items".to_string());
        let mut projector = RuntimeEventProjector::default();
        let mut projected = Vec::new();

        for (identity, text) in [
            (&first_identity, "first response"),
            (&second_identity, "second response"),
        ] {
            projected.extend(projector.project_line(&runtime_line(
                factory.assistant_message_delta(identity, text),
            )));
            let completed = CompletedModelResponse::new(
                identity.clone(),
                Some(text.to_string()),
                None,
                Vec::new(),
            );
            projected.extend(
                projector.project_line(&runtime_line(factory.model_response_completed(&completed))),
            );
        }

        let completed = projected
            .iter()
            .filter_map(|event| match event {
                ServerEvent::ItemCompleted { item, .. } if item["type"] == "agent_message" => {
                    Some(item.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0]["text"], "first response");
        assert_eq!(completed[1]["text"], "second response");
        assert_eq!(
            completed[0]["id"],
            first_identity.item_ids.agent_message_item_id().as_str()
        );
        assert_eq!(
            completed[1]["id"],
            second_identity.item_ids.agent_message_item_id().as_str()
        );
        assert_ne!(completed[0]["id"], completed[1]["id"]);
    }

    #[test]
    fn malformed_completed_model_response_fails_closed() {
        let projected = RuntimeEventProjector::default().project_line(
            r#"{"version":"1","run_id":"thread-model-items","seq":0,"timestamp_ms":1,"type":"model.response.completed","payload":{"identity":{"turn_id":"not-a-turn"}}}"#,
        );

        assert!(
            projected.iter().all(|event| !matches!(
                event,
                ServerEvent::ItemStarted { .. } | ServerEvent::ItemCompleted { .. }
            )),
            "malformed canonical completion must not fabricate item lifecycle events: {projected:?}"
        );
    }

    #[test]
    fn unified_exec_tools_are_projected_as_builtins() {
        assert!(is_builtin_tool("exec_command"));
        assert!(is_builtin_tool("write_stdin"));
        assert!(!is_dynamic_tool("exec_command"));
        assert!(!is_dynamic_tool("write_stdin"));
    }
}
