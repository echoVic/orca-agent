use std::io::{self, Write};

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub struct ServerEventEnvelope {
    pub id: Value,
    pub event: ServerEvent,
}

impl Serialize for ServerEventEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut value = serde_json::to_value(&self.event).map_err(serde::ser::Error::custom)?;
        let Value::Object(ref mut object) = value else {
            return value.serialize(serializer);
        };
        object.insert("id".to_string(), self.id.clone());
        if let ServerEvent::CommandExecOutputDelta {
            process_id,
            stream,
            delta_base64,
            cap_reached,
            ..
        } = &self.event
        {
            object.insert(
                "method".to_string(),
                Value::from("command/exec/outputDelta"),
            );
            object.insert(
                "params".to_string(),
                json!({
                    "processId": process_id,
                    "stream": stream,
                    "deltaBase64": delta_base64,
                    "capReached": cap_reached,
                }),
            );
        }
        if let ServerEvent::WorkflowLifecycle { event_name, .. } = &self.event {
            object.insert("event".to_string(), Value::from(event_name.clone()));
            object.remove("eventName");
        }
        match &self.event {
            ServerEvent::FuzzyFileSearchSessionUpdated {
                session_id,
                query,
                files,
                phase,
                progress,
            } => {
                object.insert(
                    "method".to_string(),
                    Value::from("fuzzyFileSearch/sessionUpdated"),
                );
                object.insert(
                    "params".to_string(),
                    json!({
                        "sessionId": session_id,
                        "query": query,
                        "files": files,
                        "phase": phase,
                        "progress": progress,
                    }),
                );
            }
            ServerEvent::FuzzyFileSearchSessionCompleted { session_id, query } => {
                object.insert(
                    "method".to_string(),
                    Value::from("fuzzyFileSearch/sessionCompleted"),
                );
                object.insert(
                    "params".to_string(),
                    json!({"sessionId": session_id, "query": query}),
                );
            }
            ServerEvent::MentionSearchSessionUpdated {
                session_id,
                query,
                candidates,
                phase,
                progress,
                errors,
            } => {
                object.insert("method".to_string(), Value::from("mention/search/updated"));
                object.insert(
                    "params".to_string(),
                    json!({
                        "sessionId": session_id,
                        "query": query,
                        "candidates": candidates,
                        "phase": phase,
                        "progress": progress,
                        "errors": errors,
                    }),
                );
            }
            ServerEvent::MentionSearchSessionCompleted { session_id, query } => {
                object.insert(
                    "method".to_string(),
                    Value::from("mention/search/completed"),
                );
                object.insert(
                    "params".to_string(),
                    json!({"sessionId": session_id, "query": query}),
                );
            }
            _ => {}
        }
        let mut map = serializer.serialize_map(Some(object.len()))?;
        for (key, value) in object {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    ThreadStarted {
        #[serde(rename = "threadId")]
        thread_id: Value,
    },
    TurnStarted {
        #[serde(rename = "turnId")]
        turn_id: Value,
        turn: Value,
        task: Value,
    },
    TaskStatusUpdated {
        task: Value,
    },
    ReasoningDelta {
        text: Value,
    },
    MessageDelta {
        text: Value,
    },
    ToolRequested {
        tool: Value,
        target: Value,
    },
    ToolCompleted {
        #[serde(rename = "toolCallId", skip_serializing_if = "Value::is_null")]
        tool_call_id: Value,
        tool: Value,
        status: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        output: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        error: Value,
        #[serde(rename = "exitCode", skip_serializing_if = "Value::is_null")]
        exit_code: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        kind: Value,
        #[serde(rename = "terminalSource", skip_serializing_if = "Value::is_null")]
        terminal_source: Value,
    },
    WorkflowStarted {
        #[serde(rename = "taskId")]
        task_id: Value,
        #[serde(rename = "runId")]
        run_id: Value,
        #[serde(rename = "workflowName")]
        workflow_name: Value,
        task: Value,
    },
    WorkflowResultAvailable {
        #[serde(rename = "taskId")]
        task_id: Value,
        #[serde(rename = "runId")]
        run_id: Value,
        result: Value,
        task: Value,
    },
    WorkflowCompleted {
        #[serde(rename = "taskId")]
        task_id: Value,
        #[serde(rename = "runId")]
        run_id: Value,
        #[serde(rename = "workflowName")]
        workflow_name: Value,
        task: Value,
    },
    WorkflowFailed {
        #[serde(rename = "taskId")]
        task_id: Value,
        #[serde(rename = "runId")]
        run_id: Value,
        error: Value,
        task: Value,
    },
    WorkflowLifecycle {
        #[serde(rename = "eventName")]
        event_name: String,
        #[serde(rename = "taskId")]
        task_id: Value,
        #[serde(rename = "runId")]
        run_id: Value,
        #[serde(rename = "workflowName", skip_serializing_if = "Value::is_null")]
        workflow_name: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        phase: Value,
        #[serde(rename = "agentId", skip_serializing_if = "Value::is_null")]
        agent_id: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        status: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        summary: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        output: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        error: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        reason: Value,
        task: Value,
    },
    ThreadRead {
        #[serde(rename = "threadId")]
        thread_id: Value,
        title: Value,
        cwd: Value,
        #[serde(rename = "storageHealth")]
        storage_health: Value,
        #[serde(rename = "storageHealthIssue")]
        storage_health_issue: Value,
        #[serde(rename = "sourceFingerprint")]
        source_fingerprint: Value,
        #[serde(rename = "storageIdentity")]
        storage_identity: Value,
        #[serde(rename = "runtimeWorkspaceRoots")]
        runtime_workspace_roots: Value,
        #[serde(rename = "activePermissionProfile")]
        active_permission_profile: Value,
        #[serde(rename = "additionalWorkingDirectories")]
        additional_working_directories: Value,
        #[serde(rename = "additionalWorkingDirectoryCount")]
        additional_working_directory_count: Value,
        #[serde(rename = "networkDomainPermissions")]
        network_domain_permissions: Value,
        #[serde(rename = "networkDomainPermissionCount")]
        network_domain_permission_count: Value,
        #[serde(rename = "messageCount")]
        message_count: Value,
        messages: Value,
        turns: Value,
    },
    ThreadList {
        data: Value,
        #[serde(rename = "nextCursor")]
        next_cursor: Value,
        #[serde(rename = "backwardsCursor")]
        backwards_cursor: Value,
    },
    ThreadSearch {
        data: Value,
        #[serde(rename = "diagnostics")]
        diagnostics: Value,
        #[serde(rename = "nextCursor")]
        next_cursor: Value,
        #[serde(rename = "backwardsCursor")]
        backwards_cursor: Value,
    },
    ThreadTurnsList {
        data: Value,
        #[serde(rename = "nextCursor")]
        next_cursor: Value,
        #[serde(rename = "backwardsCursor")]
        backwards_cursor: Value,
    },
    ThreadItemsList {
        data: Value,
        #[serde(rename = "nextCursor")]
        next_cursor: Value,
        #[serde(rename = "backwardsCursor")]
        backwards_cursor: Value,
    },
    ThreadMetadataUpdated {
        #[serde(rename = "threadId")]
        thread_id: Value,
        title: Value,
    },
    ThreadQueueSnapshot {
        #[serde(rename = "threadId")]
        thread_id: Value,
        snapshot: Value,
    },
    TurnControlled {
        action: Value,
        #[serde(rename = "turnId")]
        turn_id: Value,
        status: Value,
        input: Value,
    },
    PermissionRequest {
        #[serde(rename = "requestId")]
        request_id: Value,
        #[serde(rename = "threadId")]
        thread_id: Value,
        #[serde(rename = "turnId")]
        turn_id: Value,
        reason: Value,
        permissions: Value,
        command: Value,
        cwd: Value,
        capability: Value,
    },
    PermissionResolved {
        #[serde(rename = "requestId")]
        request_id: Value,
        decision: Value,
        scope: Value,
        #[serde(rename = "strictAutoReview")]
        strict_auto_review: Value,
    },
    UserInputRequest {
        #[serde(rename = "requestId")]
        request_id: Value,
        #[serde(rename = "threadId")]
        thread_id: Value,
        #[serde(rename = "turnId")]
        turn_id: Value,
        question: Value,
        choices: Value,
    },
    UserInputResolved {
        #[serde(rename = "requestId")]
        request_id: Value,
        answered: Value,
    },
    McpElicitationRequest {
        #[serde(rename = "requestId")]
        request_id: Value,
        #[serde(rename = "threadId")]
        thread_id: Value,
        #[serde(rename = "turnId")]
        turn_id: Value,
        #[serde(rename = "serverName")]
        server_name: Value,
        mode: Value,
        message: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        url: Value,
        #[serde(rename = "requestedSchema", skip_serializing_if = "Value::is_null")]
        requested_schema: Value,
    },
    McpElicitationResolved {
        #[serde(rename = "requestId")]
        request_id: Value,
        accepted: Value,
    },
    TurnPlanUpdated {
        #[serde(rename = "threadId")]
        thread_id: Value,
        #[serde(rename = "turnId")]
        turn_id: Value,
        explanation: Value,
        plan: Value,
    },
    ItemStarted {
        #[serde(rename = "threadId")]
        thread_id: Value,
        #[serde(rename = "turnId")]
        turn_id: Value,
        item: Value,
    },
    ItemMessageDelta {
        #[serde(rename = "itemId")]
        item_id: Value,
        delta: Value,
    },
    ItemPlanDelta {
        #[serde(rename = "itemId")]
        item_id: Value,
        delta: Value,
    },
    ItemReasoningDelta {
        #[serde(rename = "itemId")]
        item_id: Value,
        delta: Value,
    },
    ItemCompleted {
        #[serde(rename = "threadId")]
        thread_id: Value,
        #[serde(rename = "turnId")]
        turn_id: Value,
        item: Value,
    },
    ShellStarted {
        #[serde(rename = "shellId")]
        shell_id: Value,
        #[serde(rename = "taskId")]
        task_id: Value,
        command: Value,
        status: Value,
        #[serde(rename = "requestedTerminalMode")]
        requested_terminal_mode: Value,
        #[serde(rename = "effectiveTerminalMode")]
        effective_terminal_mode: Value,
    },
    ShellCapabilities {
        platform: Value,
        #[serde(rename = "supportsPty")]
        supports_pty: Value,
        #[serde(rename = "supportsPtyResize")]
        supports_pty_resize: Value,
        #[serde(rename = "supportedTerminalModes")]
        supported_terminal_modes: Value,
        #[serde(rename = "fallbackTerminalMode")]
        fallback_terminal_mode: Value,
        #[serde(rename = "commandExecStreamingRequiresProcessId")]
        command_exec_streaming_requires_process_id: Value,
    },
    ShellUpdated {
        #[serde(rename = "shellId")]
        shell_id: Value,
        status: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        cols: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        rows: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        stdout: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        stderr: Value,
        #[serde(rename = "exitCode", skip_serializing_if = "Value::is_null")]
        exit_code: Value,
        #[serde(rename = "capReached", skip_serializing_if = "Value::is_null")]
        cap_reached: Value,
        #[serde(skip_serializing_if = "Value::is_null")]
        description: Value,
    },
    ShellOutputDelta {
        #[serde(rename = "shellId")]
        shell_id: Value,
        stream: Value,
        delta: Value,
        #[serde(rename = "capReached")]
        cap_reached: Value,
        #[serde(rename = "final")]
        final_chunk: Value,
    },
    ShellExited {
        #[serde(rename = "shellId")]
        shell_id: Value,
        #[serde(rename = "taskId")]
        task_id: Value,
        status: Value,
        #[serde(rename = "exitCode")]
        exit_code: Value,
    },
    ShellListed {
        shells: Value,
    },
    ShellCompleted {
        #[serde(rename = "shellId")]
        shell_id: Value,
        #[serde(rename = "taskId")]
        task_id: Value,
        status: Value,
        stdout: Value,
        stderr: Value,
        #[serde(rename = "exitCode")]
        exit_code: Value,
        #[serde(rename = "capReached", skip_serializing_if = "Value::is_null")]
        cap_reached: Value,
    },
    CommandExecStarted {
        #[serde(rename = "processId")]
        process_id: Value,
    },
    CommandExecListed {
        processes: Value,
    },
    CommandExecTerminated {
        #[serde(rename = "processId")]
        process_id: Value,
    },
    CommandExecWritten {
        #[serde(rename = "processId")]
        process_id: Value,
    },
    CommandExecRead {
        #[serde(rename = "processId")]
        process_id: Value,
        status: Value,
    },
    CommandExecResized {
        #[serde(rename = "processId")]
        process_id: Value,
        cols: Value,
        rows: Value,
    },
    CommandExecOutputDelta {
        #[serde(rename = "processId")]
        process_id: Value,
        stream: Value,
        delta: Value,
        #[serde(rename = "deltaBase64", skip_serializing_if = "Value::is_null")]
        delta_base64: Value,
        #[serde(rename = "capReached")]
        cap_reached: Value,
        #[serde(rename = "final")]
        final_chunk: Value,
    },
    CommandExecCompleted {
        #[serde(rename = "processId", skip_serializing_if = "Value::is_null")]
        process_id: Value,
        #[serde(rename = "exitCode")]
        exit_code: Value,
        stdout: Value,
        stderr: Value,
    },
    FuzzyFileSearchSessionStarted {
        #[serde(rename = "sessionId")]
        session_id: Value,
    },
    FuzzyFileSearchSessionUpdateAccepted {
        #[serde(rename = "sessionId")]
        session_id: Value,
        query: Value,
    },
    FuzzyFileSearchSessionUpdated {
        #[serde(rename = "sessionId")]
        session_id: String,
        query: String,
        files: Value,
        phase: Value,
        progress: Value,
    },
    FuzzyFileSearchSessionCompleted {
        #[serde(rename = "sessionId")]
        session_id: String,
        query: String,
    },
    FuzzyFileSearchSessionStopped {
        #[serde(rename = "sessionId")]
        session_id: Value,
    },
    MentionSearchSessionStarted {
        #[serde(rename = "sessionId")]
        session_id: Value,
        #[serde(rename = "threadId")]
        thread_id: Value,
    },
    MentionSearchSessionUpdateAccepted {
        #[serde(rename = "sessionId")]
        session_id: Value,
        query: Value,
    },
    MentionSearchSessionUpdated {
        #[serde(rename = "sessionId")]
        session_id: String,
        query: String,
        candidates: Value,
        phase: Value,
        progress: Value,
        errors: Value,
    },
    MentionSearchSessionCompleted {
        #[serde(rename = "sessionId")]
        session_id: String,
        query: String,
    },
    MentionSearchSessionStopped {
        #[serde(rename = "sessionId")]
        session_id: Value,
    },
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<&'static str>,
        #[serde(rename = "storageHealth", skip_serializing_if = "Value::is_null")]
        storage_health: Value,
        #[serde(rename = "storageHealthIssue", skip_serializing_if = "Value::is_null")]
        storage_health_issue: Value,
    },
    TurnCompleted {
        status: Value,
    },
}

impl ServerEvent {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
            code: None,
            storage_health: Value::Null,
            storage_health_issue: Value::Null,
        }
    }

    pub fn stored_session_health_error(
        message: impl Into<String>,
        health: crate::thread_store::StoredSessionHealth,
        issue: Option<crate::thread_store::StoredSessionHealthIssue>,
    ) -> Self {
        Self::Error {
            message: message.into(),
            code: Some("stored_session_health"),
            storage_health: serde_json::to_value(health).unwrap_or(Value::Null),
            storage_health_issue: serde_json::to_value(issue).unwrap_or(Value::Null),
        }
    }
}

pub fn map_runtime_event_line(line: &str) -> Option<ServerEvent> {
    let event: Value = serde_json::from_str(line).ok()?;
    let payload = &event["payload"];
    match event["type"].as_str()? {
        "turn.started" => Some(ServerEvent::TurnStarted {
            turn_id: payload["turn_id"].clone(),
            turn: payload["turn"].clone(),
            task: payload["task"].clone(),
        }),
        "task.status.updated" => Some(ServerEvent::TaskStatusUpdated {
            task: payload["task"].clone(),
        }),
        "assistant.reasoning.delta" => Some(ServerEvent::ReasoningDelta {
            text: payload["text"].clone(),
        }),
        "assistant.message.delta" => Some(ServerEvent::MessageDelta {
            text: payload["text"].clone(),
        }),
        "plan.updated" => Some(ServerEvent::TurnPlanUpdated {
            thread_id: Value::Null,
            turn_id: Value::Null,
            explanation: payload["explanation"].clone(),
            plan: payload["plan"].clone(),
        }),
        "tool.call.requested" => Some(ServerEvent::ToolRequested {
            tool: payload["name"].clone(),
            target: payload["target"].clone(),
        }),
        "tool.call.completed" => Some(ServerEvent::ToolCompleted {
            tool_call_id: payload["id"].clone(),
            tool: payload["name"].clone(),
            status: payload["status"].clone(),
            output: payload["output"].clone(),
            error: payload["error"].clone(),
            exit_code: payload["exit_code"].clone(),
            kind: payload["kind"].clone(),
            terminal_source: payload["terminal_source"].clone(),
        }),
        "surface.permission.requested" => Some(ServerEvent::PermissionRequest {
            request_id: payload["request_id"].clone(),
            thread_id: payload["thread_id"].clone(),
            turn_id: payload["turn_id"].clone(),
            reason: payload["reason"].clone(),
            permissions: payload["permissions"].clone(),
            command: payload["command"].clone(),
            cwd: payload["cwd"].clone(),
            capability: payload["capability"].clone(),
        }),
        "surface.user_input.requested" => Some(ServerEvent::UserInputRequest {
            request_id: payload["request_id"].clone(),
            thread_id: payload["thread_id"].clone(),
            turn_id: payload["turn_id"].clone(),
            question: payload["question"].clone(),
            choices: payload["choices"].clone(),
        }),
        "surface.mcp_elicitation.requested" => Some(ServerEvent::McpElicitationRequest {
            request_id: payload["request_id"].clone(),
            thread_id: payload["thread_id"].clone(),
            turn_id: payload["turn_id"].clone(),
            server_name: payload["server_name"].clone(),
            mode: payload["mode"].clone(),
            message: payload["message"].clone(),
            url: payload["url"].clone(),
            requested_schema: payload["requested_schema"].clone(),
        }),
        "workflow.started" => Some(ServerEvent::WorkflowStarted {
            task_id: payload["taskId"].clone(),
            run_id: payload["runId"].clone(),
            workflow_name: payload["workflowName"].clone(),
            task: payload["task"].clone(),
        }),
        "workflow.result.available" => Some(ServerEvent::WorkflowResultAvailable {
            task_id: payload["taskId"].clone(),
            run_id: payload["runId"].clone(),
            result: payload["result"].clone(),
            task: payload["task"].clone(),
        }),
        "workflow.completed" => Some(ServerEvent::WorkflowCompleted {
            task_id: payload["taskId"].clone(),
            run_id: payload["runId"].clone(),
            workflow_name: payload["workflowName"].clone(),
            task: payload["task"].clone(),
        }),
        "workflow.failed" => Some(ServerEvent::WorkflowFailed {
            task_id: payload["taskId"].clone(),
            run_id: payload["runId"].clone(),
            error: payload["error"].clone(),
            task: payload["task"].clone(),
        }),
        "workflow.resumed"
        | "workflow.phase.started"
        | "workflow.phase.completed"
        | "workflow.agent.started"
        | "workflow.agent.cached"
        | "workflow.agent.completed"
        | "workflow.agent.failed"
        | "workflow.paused"
        | "workflow.stopped" => Some(ServerEvent::WorkflowLifecycle {
            event_name: event["type"].as_str().unwrap_or_default().replace('.', "_"),
            task_id: payload["taskId"].clone(),
            run_id: payload["runId"].clone(),
            workflow_name: payload["workflowName"].clone(),
            phase: payload["phase"].clone(),
            agent_id: payload["agentId"].clone(),
            status: payload["status"].clone(),
            summary: payload["summary"].clone(),
            output: payload["output"].clone(),
            error: payload["error"].clone(),
            reason: payload["reason"].clone(),
            task: payload["task"].clone(),
        }),
        "error" => Some(ServerEvent::Error {
            message: payload["message"].as_str().unwrap_or_default().to_string(),
            code: None,
            storage_health: Value::Null,
            storage_health_issue: Value::Null,
        }),
        "session.completed" => Some(ServerEvent::TurnCompleted {
            status: payload["status"].clone(),
        }),
        _ => None,
    }
}

pub fn write_server_event<W: Write>(
    writer: &mut W,
    id: &Value,
    event: ServerEvent,
) -> io::Result<()> {
    // Serialize the envelope and its trailing newline into one buffer so the
    // underlying writer (which may be a `LockedServerWriter` sharing a Mutex
    // with other event producers) writes the complete line under a single
    // lock acquisition. Two separate `to_writer`/`writeln` calls would let
    // another thread interleave its own JSON between them.
    let mut bytes = Vec::new();
    serde_json::to_writer(
        &mut bytes,
        &ServerEventEnvelope {
            id: id.clone(),
            event,
        },
    )?;
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn legacy_json_event(id: Value, event: ServerEvent) -> Value {
    serde_json::to_value(ServerEventEnvelope { id, event }).unwrap_or_else(|error| {
        json!({
            "id": null,
            "event": "error",
            "message": format!("failed to encode protocol event: {error}")
        })
    })
}
