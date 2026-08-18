pub(crate) use orca_core::thread_item_projection::{
    ProjectedPersistedMessageThreadItem, ProjectedUserMessageThreadItem,
};
use serde::Serialize;
use serde_json::{Value, json};

pub(crate) fn mcp_tool_parts(tool: &str) -> Option<(String, String)> {
    let rest = tool.strip_prefix("mcp__")?;
    let (server, local_tool) = rest.rsplit_once("__")?;
    Some((server.to_string(), local_tool.to_string()))
}

pub(crate) fn parse_json_or_null(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

pub(crate) fn mcp_result_from_content(content: &str) -> Value {
    match serde_json::from_str::<Value>(content) {
        Ok(value) if value.is_object() => json!({
            "content": value.get("content").cloned().unwrap_or_else(|| {
                json!([{ "type": "text", "text": content }])
            }),
            "structuredContent": value.get("structuredContent").cloned().unwrap_or(Value::Null),
            "_meta": value.get("_meta").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({
            "content": [{ "type": "text", "text": content }],
            "structuredContent": Value::Null,
            "_meta": Value::Null,
        }),
    }
}

pub(crate) fn mcp_tool_started_item(
    id: impl Into<String>,
    server: impl Into<String>,
    tool: impl Into<String>,
    arguments: Value,
) -> Value {
    ProjectedThreadItem::from(ProjectedMcpToolThreadItem::started(
        id, server, tool, arguments,
    ))
    .into_value()
}

pub(crate) fn dynamic_tool_started_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    arguments: Value,
) -> Value {
    ProjectedThreadItem::from(ProjectedDynamicToolThreadItem::started(id, tool, arguments))
        .into_value()
}

pub(crate) fn mcp_tool_completed_item(
    id: impl Into<String>,
    server: impl Into<String>,
    tool: impl Into<String>,
    status: impl Into<String>,
    arguments: Value,
    result: Value,
    error: Value,
) -> Value {
    ProjectedThreadItem::from(ProjectedMcpToolThreadItem::completed(
        ProjectedMcpToolCompletion {
            id: id.into(),
            server: server.into(),
            tool: tool.into(),
            status: status.into(),
            arguments,
            result,
            error,
        },
    ))
    .into_value()
}

pub(crate) fn dynamic_tool_completed_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    status: impl Into<String>,
    arguments: Value,
    content_items: Value,
    success: bool,
    error: Value,
) -> Value {
    ProjectedThreadItem::from(ProjectedDynamicToolThreadItem::completed(
        ProjectedDynamicToolCompletion {
            id: id.into(),
            tool: tool.into(),
            status: status.into(),
            arguments,
            content_items,
            success,
            error,
        },
    ))
    .into_value()
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum ProjectedThreadItem {
    UserMessage(ProjectedUserMessageThreadItem),
    PersistedMessage(ProjectedPersistedMessageThreadItem),
    CommandExecution(ProjectedCommandExecutionThreadItem),
    McpTool(ProjectedMcpToolThreadItem),
    DynamicTool(ProjectedDynamicToolThreadItem),
    FileChange(ProjectedFileChangeThreadItem),
    Workflow(ProjectedWorkflowThreadItem),
}

impl ProjectedThreadItem {
    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected thread item serializes")
    }
}

impl From<ProjectedUserMessageThreadItem> for ProjectedThreadItem {
    fn from(item: ProjectedUserMessageThreadItem) -> Self {
        Self::UserMessage(item)
    }
}

impl From<ProjectedPersistedMessageThreadItem> for ProjectedThreadItem {
    fn from(item: ProjectedPersistedMessageThreadItem) -> Self {
        Self::PersistedMessage(item)
    }
}

impl From<ProjectedCommandExecutionThreadItem> for ProjectedThreadItem {
    fn from(item: ProjectedCommandExecutionThreadItem) -> Self {
        Self::CommandExecution(item)
    }
}

impl From<ProjectedMcpToolThreadItem> for ProjectedThreadItem {
    fn from(item: ProjectedMcpToolThreadItem) -> Self {
        Self::McpTool(item)
    }
}

impl From<ProjectedDynamicToolThreadItem> for ProjectedThreadItem {
    fn from(item: ProjectedDynamicToolThreadItem) -> Self {
        Self::DynamicTool(item)
    }
}

impl From<ProjectedFileChangeThreadItem> for ProjectedThreadItem {
    fn from(item: ProjectedFileChangeThreadItem) -> Self {
        Self::FileChange(item)
    }
}

impl From<ProjectedWorkflowThreadItem> for ProjectedThreadItem {
    fn from(item: ProjectedWorkflowThreadItem) -> Self {
        Self::Workflow(item)
    }
}

pub(crate) fn persisted_message_thread_item(item: ProjectedPersistedMessageThreadItem) -> Value {
    ProjectedThreadItem::from(item).into_value()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectedToolCallCompletion {
    pub(crate) status: String,
    pub(crate) command_status: Value,
    pub(crate) arguments: Value,
    pub(crate) result: Value,
    pub(crate) command_error: Value,
    pub(crate) mcp_error: Value,
    pub(crate) dynamic_error: Value,
    pub(crate) content_items: Value,
    pub(crate) success: bool,
    pub(crate) aggregated_output: Value,
    pub(crate) exit_code: Value,
    pub(crate) truncated: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ProjectedToolCallItem {
    CommandExecution {
        id: String,
        tool: String,
        command: Option<String>,
    },
    McpTool {
        id: String,
        server: String,
        tool: String,
    },
    DynamicTool {
        id: String,
        tool: String,
    },
}

impl ProjectedToolCallItem {
    pub(crate) fn command_execution(
        id: impl Into<String>,
        tool: impl Into<String>,
        command: Option<impl Into<String>>,
    ) -> Self {
        Self::CommandExecution {
            id: id.into(),
            tool: tool.into(),
            command: command.map(Into::into),
        }
    }

    pub(crate) fn mcp_tool(
        id: impl Into<String>,
        server: impl Into<String>,
        tool: impl Into<String>,
    ) -> Self {
        Self::McpTool {
            id: id.into(),
            server: server.into(),
            tool: tool.into(),
        }
    }

    pub(crate) fn dynamic_tool(id: impl Into<String>, tool: impl Into<String>) -> Self {
        Self::DynamicTool {
            id: id.into(),
            tool: tool.into(),
        }
    }

    pub(crate) fn started_item(&self, arguments: Value) -> Value {
        match self {
            Self::CommandExecution { id, tool, command } => {
                command_execution_started_item(id.clone(), tool.clone(), command.clone())
            }
            Self::McpTool { id, server, tool } => {
                mcp_tool_started_item(id.clone(), server.clone(), tool.clone(), arguments)
            }
            Self::DynamicTool { id, tool } => {
                dynamic_tool_started_item(id.clone(), tool.clone(), arguments)
            }
        }
    }

    pub(crate) fn completed_item(self, completion: ProjectedToolCallCompletion) -> Value {
        match self {
            Self::CommandExecution { id, tool, command } => command_execution_completed_item(
                id,
                tool,
                command,
                completion.command_status,
                completion.aggregated_output,
                completion.command_error,
                completion.exit_code,
                completion.truncated,
            ),
            Self::McpTool { id, server, tool } => mcp_tool_completed_item(
                id,
                server,
                tool,
                completion.status,
                completion.arguments,
                completion.result,
                completion.mcp_error,
            ),
            Self::DynamicTool { id, tool } => dynamic_tool_completed_item(
                id,
                tool,
                completion.status,
                completion.arguments,
                completion.content_items,
                completion.success,
                completion.dynamic_error,
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectedMcpToolCompletion {
    id: String,
    server: String,
    tool: String,
    status: String,
    arguments: Value,
    result: Value,
    error: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub(crate) enum ProjectedMcpToolThreadItem {
    #[serde(rename = "mcpToolCall")]
    Started {
        id: String,
        server: String,
        tool: String,
        status: String,
        arguments: Value,
        result: Value,
        error: Value,
    },
    #[serde(rename = "mcpToolCall")]
    Completed {
        id: String,
        server: String,
        tool: String,
        status: String,
        arguments: Value,
        result: Value,
        error: Value,
    },
}

impl ProjectedMcpToolThreadItem {
    pub(crate) fn started(
        id: impl Into<String>,
        server: impl Into<String>,
        tool: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self::Started {
            id: id.into(),
            server: server.into(),
            tool: tool.into(),
            status: "in_progress".to_string(),
            arguments,
            result: Value::Null,
            error: Value::Null,
        }
    }

    pub(crate) fn completed(completion: ProjectedMcpToolCompletion) -> Self {
        Self::Completed {
            id: completion.id,
            server: completion.server,
            tool: completion.tool,
            status: completion.status,
            arguments: completion.arguments,
            result: completion.result,
            error: completion.error,
        }
    }

    #[cfg(test)]
    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected mcp tool thread item serializes")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectedDynamicToolCompletion {
    id: String,
    tool: String,
    status: String,
    arguments: Value,
    content_items: Value,
    success: bool,
    error: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub(crate) enum ProjectedDynamicToolThreadItem {
    #[serde(rename = "dynamicToolCall")]
    Started {
        id: String,
        namespace: Value,
        tool: String,
        status: String,
        arguments: Value,
        #[serde(rename = "contentItems")]
        content_items: Value,
        success: Value,
        error: Value,
    },
    #[serde(rename = "dynamicToolCall")]
    Completed {
        id: String,
        namespace: Value,
        tool: String,
        status: String,
        arguments: Value,
        #[serde(rename = "contentItems")]
        content_items: Value,
        success: bool,
        error: Value,
    },
}

impl ProjectedDynamicToolThreadItem {
    pub(crate) fn started(
        id: impl Into<String>,
        tool: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self::Started {
            id: id.into(),
            namespace: Value::Null,
            tool: tool.into(),
            status: "in_progress".to_string(),
            arguments,
            content_items: Value::Null,
            success: Value::Null,
            error: Value::Null,
        }
    }

    pub(crate) fn completed(completion: ProjectedDynamicToolCompletion) -> Self {
        Self::Completed {
            id: completion.id,
            namespace: Value::Null,
            tool: completion.tool,
            status: completion.status,
            arguments: completion.arguments,
            content_items: completion.content_items,
            success: completion.success,
            error: completion.error,
        }
    }

    #[cfg(test)]
    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected dynamic tool thread item serializes")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectedFileChangeCompletion {
    id: String,
    path: Option<String>,
    kind: String,
    status: Value,
    diff: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectedFileChangeItem {
    id: String,
    path: Option<String>,
    kind: String,
}

impl ProjectedFileChangeItem {
    pub(crate) fn new(
        id: impl Into<String>,
        path: Option<impl Into<String>>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            path: path.map(Into::into),
            kind: kind.into(),
        }
    }

    pub(crate) fn started_item(&self, diff: Value) -> Value {
        file_change_started_item(self.id.clone(), self.path.clone(), self.kind.clone(), diff)
    }

    pub(crate) fn completed_item(self, status: Value, diff: Value) -> Value {
        file_change_completed_item(self.id, self.path, self.kind, status, diff)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ProjectedFileChange {
    path: Option<String>,
    kind: String,
    diff: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub(crate) enum ProjectedFileChangeThreadItem {
    #[serde(rename = "fileChange")]
    Started {
        id: String,
        status: Value,
        changes: Vec<ProjectedFileChange>,
    },
    #[serde(rename = "fileChange")]
    Completed {
        id: String,
        status: Value,
        changes: Vec<ProjectedFileChange>,
    },
}

impl ProjectedFileChangeThreadItem {
    pub(crate) fn started(
        id: impl Into<String>,
        path: Option<impl Into<String>>,
        kind: impl Into<String>,
        diff: Value,
    ) -> Self {
        Self::Started {
            id: id.into(),
            status: Value::from("inProgress"),
            changes: vec![ProjectedFileChange {
                path: path.map(Into::into),
                kind: kind.into(),
                diff,
            }],
        }
    }

    pub(crate) fn completed(completion: ProjectedFileChangeCompletion) -> Self {
        Self::Completed {
            id: completion.id,
            status: completion.status,
            changes: vec![ProjectedFileChange {
                path: completion.path,
                kind: completion.kind,
                diff: completion.diff,
            }],
        }
    }

    #[cfg(test)]
    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected file change thread item serializes")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectedWorkflowCompletion {
    id: String,
    task_id: String,
    workflow_name: String,
    status: String,
    result: Value,
    error: Value,
    task: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectedWorkflowItem {
    id: String,
    task_id: String,
    workflow_name: String,
    task: Value,
    status: String,
    result: Value,
}

impl ProjectedWorkflowItem {
    pub(crate) fn started(
        id: impl Into<String>,
        task_id: impl Into<String>,
        workflow_name: impl Into<String>,
        task: Value,
    ) -> Self {
        Self {
            id: id.into(),
            task_id: task_id.into(),
            workflow_name: workflow_name.into(),
            task,
            status: "running".to_string(),
            result: Value::Null,
        }
    }

    pub(crate) fn started_item(&self) -> Value {
        workflow_started_item(
            self.id.clone(),
            self.task_id.clone(),
            self.workflow_name.clone(),
            self.task.clone(),
        )
    }

    pub(crate) fn record_result(&mut self, result: Value, task: Value) {
        self.result = result;
        self.task = task;
        self.status = "completed".to_string();
    }

    pub(crate) fn record_completed(&mut self, task: Value) {
        self.task = task;
        self.status = "completed".to_string();
    }

    pub(crate) fn fill_task_if_missing(&mut self, task: Value) {
        if self.task.is_null() {
            self.task = task;
        }
    }

    pub(crate) fn completed_item(self, status: impl Into<String>, error: Value) -> Value {
        workflow_completed_item(
            self.id,
            self.task_id,
            self.workflow_name,
            status,
            self.result,
            error,
            self.task,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub(crate) enum ProjectedWorkflowThreadItem {
    #[serde(rename = "workflow")]
    Started {
        id: String,
        #[serde(rename = "workflowName")]
        workflow_name: String,
        #[serde(rename = "taskId")]
        task_id: String,
        status: String,
        task: Value,
    },
    #[serde(rename = "workflow")]
    Completed {
        id: String,
        #[serde(rename = "workflowName")]
        workflow_name: String,
        #[serde(rename = "taskId")]
        task_id: String,
        status: String,
        result: Value,
        error: Value,
        task: Value,
    },
}

impl ProjectedWorkflowThreadItem {
    pub(crate) fn started(
        id: impl Into<String>,
        task_id: impl Into<String>,
        workflow_name: impl Into<String>,
        task: Value,
    ) -> Self {
        Self::Started {
            id: id.into(),
            workflow_name: workflow_name.into(),
            task_id: task_id.into(),
            status: "running".to_string(),
            task,
        }
    }

    pub(crate) fn completed(completion: ProjectedWorkflowCompletion) -> Self {
        Self::Completed {
            id: completion.id,
            workflow_name: completion.workflow_name,
            task_id: completion.task_id,
            status: completion.status,
            result: completion.result,
            error: completion.error,
            task: completion.task,
        }
    }

    #[cfg(test)]
    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected workflow thread item serializes")
    }
}

pub(crate) fn user_message_item(content: impl Into<String>) -> Value {
    ProjectedThreadItem::from(ProjectedUserMessageThreadItem::new(content)).into_value()
}

pub(crate) fn command_execution_started_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    command: Option<impl Into<String>>,
) -> Value {
    ProjectedThreadItem::from(ProjectedCommandExecutionThreadItem::started(
        id, tool, command,
    ))
    .into_value()
}

pub(crate) fn command_execution_completed_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    command: Option<impl Into<String>>,
    status: Value,
    aggregated_output: Value,
    error: Value,
    exit_code: Value,
    truncated: Value,
) -> Value {
    ProjectedThreadItem::from(ProjectedCommandExecutionThreadItem::completed(
        ProjectedCommandExecutionCompletion {
            id: id.into(),
            tool: tool.into(),
            command: command.map(Into::into),
            status,
            aggregated_output,
            error,
            exit_code,
            truncated,
        },
    ))
    .into_value()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProjectedCommandExecutionCompletion {
    id: String,
    tool: String,
    command: Option<String>,
    status: Value,
    aggregated_output: Value,
    error: Value,
    exit_code: Value,
    truncated: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub(crate) enum ProjectedCommandExecutionThreadItem {
    #[serde(rename = "commandExecution")]
    Started {
        id: String,
        tool: String,
        command: Option<String>,
        status: String,
    },
    #[serde(rename = "commandExecution")]
    Completed {
        id: String,
        tool: String,
        command: Option<String>,
        status: Value,
        #[serde(rename = "aggregatedOutput")]
        aggregated_output: Value,
        error: Value,
        #[serde(rename = "exitCode")]
        exit_code: Value,
        truncated: Value,
    },
}

impl ProjectedCommandExecutionThreadItem {
    pub(crate) fn started(
        id: impl Into<String>,
        tool: impl Into<String>,
        command: Option<impl Into<String>>,
    ) -> Self {
        Self::Started {
            id: id.into(),
            tool: tool.into(),
            command: command.map(Into::into),
            status: "in_progress".to_string(),
        }
    }

    pub(crate) fn completed(completion: ProjectedCommandExecutionCompletion) -> Self {
        Self::Completed {
            id: completion.id,
            tool: completion.tool,
            command: completion.command,
            status: completion.status,
            aggregated_output: completion.aggregated_output,
            error: completion.error,
            exit_code: completion.exit_code,
            truncated: completion.truncated,
        }
    }

    #[cfg(test)]
    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("projected command execution thread item serializes")
    }
}

pub(crate) fn persisted_command_execution_started_item(
    id: impl Into<String>,
    tool: impl Into<String>,
    command: Value,
) -> Value {
    json!({
        "id": id.into(),
        "type": "commandExecution",
        "tool": tool.into(),
        "command": command,
        "cwd": Value::Null,
        "processId": Value::Null,
        "source": Value::Null,
        "status": "in_progress",
        "commandActions": [],
        "aggregatedOutput": Value::Null,
        "error": Value::Null,
        "exitCode": Value::Null,
        "durationMs": Value::Null,
    })
}

pub(crate) fn persisted_command_execution_completed_item(
    started: &Value,
    status: Value,
    aggregated_output: Value,
    error: Value,
    truncated: Value,
) -> Value {
    let mut item = started.clone();
    item["status"] = status;
    item["aggregatedOutput"] = aggregated_output;
    item["error"] = error;
    if !truncated.is_null() {
        item["truncated"] = truncated;
    }
    item
}

pub(crate) fn file_change_started_item(
    id: impl Into<String>,
    path: Option<impl Into<String>>,
    kind: impl Into<String>,
    diff: Value,
) -> Value {
    ProjectedThreadItem::from(ProjectedFileChangeThreadItem::started(id, path, kind, diff))
        .into_value()
}

pub(crate) fn file_change_completed_item(
    id: impl Into<String>,
    path: Option<impl Into<String>>,
    kind: impl Into<String>,
    status: Value,
    diff: Value,
) -> Value {
    ProjectedThreadItem::from(ProjectedFileChangeThreadItem::completed(
        ProjectedFileChangeCompletion {
            id: id.into(),
            path: path.map(Into::into),
            kind: kind.into(),
            status,
            diff,
        },
    ))
    .into_value()
}

pub(crate) fn persisted_file_change_started_item(
    tool_call_id: &str,
    tool: &str,
    arguments: &Value,
) -> Option<Value> {
    Some(file_change_started_item(
        format!("{tool_call_id}:file-change"),
        file_change_path(tool, arguments),
        file_change_kind(tool)?,
        Value::from(String::new()),
    ))
}

pub(crate) fn persisted_file_change_completed_item(started: &Value, status: Value) -> Value {
    let mut item = started.clone();
    item["status"] = status;
    item
}

pub(crate) fn complete_projected_tool_item(item: &mut Value, result: &Value) {
    let content = result["content"].as_str().unwrap_or_default();
    if let Some((status, failure)) = tool_failure_from_result(result)
        .or_else(|| parse_tool_failure_content(content).map(|failure| ("failed", failure)))
    {
        rebuild_completed_projected_tool_item(item, status, result, Value::Null, failure);
        return;
    }

    rebuild_completed_projected_tool_item(
        item,
        "completed",
        result,
        mcp_result_from_content(content),
        Value::Null,
    );
}

fn file_change_kind(tool: &str) -> Option<&'static str> {
    match tool {
        "edit" => Some("edit"),
        "write_file" => Some("write"),
        _ => None,
    }
}

fn file_change_path(tool: &str, arguments: &Value) -> Option<String> {
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| arguments.get("target").and_then(Value::as_str))?
        .trim();
    if path.is_empty() {
        return None;
    }
    match tool {
        "edit" | "write_file" => Some(path.to_string()),
        _ => None,
    }
}

pub(crate) fn workflow_started_item(
    id: impl Into<String>,
    task_id: impl Into<String>,
    workflow_name: impl Into<String>,
    task: Value,
) -> Value {
    ProjectedThreadItem::from(ProjectedWorkflowThreadItem::started(
        id,
        task_id,
        workflow_name,
        task,
    ))
    .into_value()
}

pub(crate) fn workflow_completed_item(
    id: impl Into<String>,
    task_id: impl Into<String>,
    workflow_name: impl Into<String>,
    status: impl Into<String>,
    result: Value,
    error: Value,
    task: Value,
) -> Value {
    ProjectedThreadItem::from(ProjectedWorkflowThreadItem::completed(
        ProjectedWorkflowCompletion {
            id: id.into(),
            task_id: task_id.into(),
            workflow_name: workflow_name.into(),
            status: status.into(),
            result,
            error,
            task,
        },
    ))
    .into_value()
}

pub(crate) fn tool_error_object(message: &str, exit_code: Option<i64>) -> Value {
    let mut error =
        serde_json::Map::from_iter([("message".to_string(), Value::from(message.to_string()))]);
    if let Some(exit_code) = exit_code {
        error.insert("exitCode".to_string(), Value::from(exit_code));
    }
    Value::Object(error)
}

pub(crate) fn tool_error_object_from_value(message: &str, value: &Value) -> Value {
    tool_error_object(
        message,
        value
            .get("exit_code")
            .and_then(Value::as_i64)
            .or_else(|| value.get("exitCode").and_then(Value::as_i64)),
    )
}

pub(crate) fn tool_status_is_completed(payload: &Value) -> bool {
    payload["status"].as_str() == Some("completed")
}

fn rebuild_completed_projected_tool_item(
    item: &mut Value,
    status: &str,
    result: &Value,
    mcp_result: Value,
    error: Value,
) {
    if item["type"] == "mcpToolCall" {
        *item = mcp_tool_completed_item(
            item["id"].as_str().unwrap_or_default(),
            item["server"].as_str().unwrap_or_default(),
            item["tool"].as_str().unwrap_or_default(),
            status,
            item["arguments"].clone(),
            mcp_result,
            error,
        );
        copy_truncated_metadata(item, result);
        copy_terminal_metadata(item, result);
        return;
    }

    if item["type"] == "dynamicToolCall" {
        let content_items = if status == "completed" {
            json!([{
                "type": "text",
                "text": result["content"].as_str().unwrap_or_default(),
            }])
        } else {
            Value::Null
        };
        *item = dynamic_tool_completed_item(
            item["id"].as_str().unwrap_or_default(),
            item["tool"].as_str().unwrap_or_default(),
            status,
            item["arguments"].clone(),
            content_items,
            status == "completed",
            error,
        );
        copy_truncated_metadata(item, result);
        copy_terminal_metadata(item, result);
        return;
    }

    if item["type"] == "commandExecution" {
        let content = result["content"].as_str().unwrap_or_default();
        let unified_exec =
            unified_exec_projection(item["tool"].as_str().unwrap_or_default(), content);
        let projected_status = unified_exec
            .as_ref()
            .map(|projection| projection.status.clone())
            .unwrap_or_else(|| Value::from(status.to_string()));
        let projected_output = unified_exec
            .as_ref()
            .map(|projection| projection.output.clone())
            .unwrap_or_else(|| {
                if status == "completed" {
                    Value::from(content.to_string())
                } else {
                    Value::Null
                }
            });
        let projected_error = unified_exec
            .as_ref()
            .map(|projection| projection.error.clone())
            .unwrap_or_else(|| {
                if status == "completed" {
                    Value::Null
                } else {
                    error
                }
            });
        let projected_truncated = unified_exec
            .as_ref()
            .map(|projection| projection.truncated.clone())
            .unwrap_or_else(|| truncated_metadata(result));
        *item = persisted_command_execution_completed_item(
            item,
            projected_status,
            projected_output,
            projected_error,
            projected_truncated,
        );
        if let Some(projection) = unified_exec {
            item["exitCode"] = projection.exit_code;
        }
        copy_terminal_metadata(item, result);
        return;
    }

    if item["type"] == "fileChange" {
        *item = persisted_file_change_completed_item(item, Value::from(status.to_string()));
        if matches!(status, "cancelled" | "indeterminate") {
            item["error"] = error;
        }
        copy_terminal_metadata(item, result);
        return;
    }

    let content = result["content"].as_str().unwrap_or_default();
    item["status"] = Value::from(status.to_string());
    copy_truncated_metadata(item, result);
    item["result"] = if status == "completed" {
        Value::from(content.to_string())
    } else {
        Value::Null
    };
    item["error"] = error;
    copy_terminal_metadata(item, result);
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UnifiedExecProjection {
    pub(crate) status: Value,
    pub(crate) output: Value,
    pub(crate) error: Value,
    pub(crate) exit_code: Value,
    pub(crate) truncated: Value,
}

pub(crate) fn unified_exec_projection(tool: &str, content: &str) -> Option<UnifiedExecProjection> {
    if !matches!(tool, "exec_command" | "write_stdin") {
        return None;
    }
    let content = content
        .strip_suffix("\n[output truncated]")
        .unwrap_or(content);
    let value: Value = serde_json::from_str(content).ok()?;
    let status = value.get("status")?.as_str()?;
    let output = value
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let error = matches!(status, "failed" | "cancelled" | "stopped")
        .then_some(output)
        .filter(|error| !error.is_empty())
        .map(|error| tool_error_object_from_value(error, &value))
        .unwrap_or(Value::Null);
    Some(UnifiedExecProjection {
        status: Value::from(status.to_string()),
        output: Value::from(output.to_string()),
        error,
        exit_code: value.get("exit_code").cloned().unwrap_or(Value::Null),
        truncated: value
            .get("truncated")
            .cloned()
            .filter(|value| value.as_bool() == Some(true))
            .unwrap_or(Value::Null),
    })
}

fn truncated_metadata(result: &Value) -> Value {
    if result["truncated"].as_bool() == Some(true) {
        Value::from(true)
    } else {
        Value::Null
    }
}

fn copy_truncated_metadata(item: &mut Value, result: &Value) {
    if result["truncated"].as_bool() == Some(true) {
        item["truncated"] = Value::from(true);
    }
}

fn copy_terminal_metadata(item: &mut Value, result: &Value) {
    for field in ["kind", "terminalSource", "invocationStarted"] {
        if let Some(value) = result.get(field) {
            item[field] = value.clone();
        }
    }
}

fn tool_failure_from_result(result: &Value) -> Option<(&'static str, Value)> {
    let status = match result["status"].as_str()? {
        "completed" => return None,
        "failed" => "failed",
        "denied" => "denied",
        "not_implemented" => "not_implemented",
        "cancelled" => "cancelled",
        "indeterminate" => "indeterminate",
        _ => "failed",
    };
    let message = result["error"]
        .as_str()
        .filter(|message| !message.is_empty())
        .or_else(|| {
            result["content"]
                .as_str()
                .filter(|message| !message.is_empty())
        })
        .unwrap_or("tool call failed");
    Some((status, tool_error_object_from_value(message, result)))
}

fn parse_tool_failure_content(content: &str) -> Option<Value> {
    if let Some(message) = content.strip_prefix("ERROR: ") {
        return Some(json!({ "message": message }));
    }

    let value = serde_json::from_str::<Value>(content).ok()?;
    if value.get("status").and_then(Value::as_str) != Some("failed") {
        return None;
    }
    let message = value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or("tool call failed");
    Some(tool_error_object_from_value(message, &value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_result_from_content_preserves_structured_payload_shape() {
        let result = mcp_result_from_content(
            r#"{"content":[{"type":"text","text":"ok"}],"structuredContent":{"answer":42},"_meta":{"trace":"abc"}}"#,
        );

        assert_eq!(result["content"][0]["text"], "ok");
        assert_eq!(result["structuredContent"]["answer"], 42);
        assert_eq!(result["_meta"]["trace"], "abc");
    }

    #[test]
    fn tool_error_object_uses_camel_case_exit_code() {
        let error = tool_error_object("failed", Some(42));

        assert_eq!(error["message"], "failed");
        assert_eq!(error["exitCode"], 42);
        assert!(error.get("exit_code").is_none());
    }

    #[test]
    fn tool_error_object_from_value_normalizes_exit_code_field_names() {
        let snake_case = tool_error_object_from_value(
            "failed",
            &json!({
                "exit_code": 17,
            }),
        );
        let camel_case = tool_error_object_from_value(
            "failed",
            &json!({
                "exitCode": 18,
            }),
        );

        assert_eq!(snake_case["exitCode"], 17);
        assert!(snake_case.get("exit_code").is_none());
        assert_eq!(camel_case["exitCode"], 18);
    }

    #[test]
    fn tool_status_is_completed_only_accepts_completed_status() {
        assert!(tool_status_is_completed(&json!({ "status": "completed" })));
        assert!(!tool_status_is_completed(&json!({ "status": "failed" })));
        assert!(!tool_status_is_completed(&json!({ "status": Value::Null })));
    }

    #[test]
    fn mcp_tool_started_item_projects_codex_style_shape() {
        let item = mcp_tool_started_item("call-1", "server", "search", json!({ "q": "orca" }));

        assert_eq!(item["id"], "call-1");
        assert_eq!(item["type"], "mcpToolCall");
        assert_eq!(item["server"], "server");
        assert_eq!(item["tool"], "search");
        assert_eq!(item["status"], "in_progress");
        assert_eq!(item["arguments"]["q"], "orca");
        assert!(item["result"].is_null());
        assert!(item["error"].is_null());
    }

    #[test]
    fn dynamic_tool_started_item_projects_codex_style_shape() {
        let item = dynamic_tool_started_item("call-2", "web_search", json!({ "query": "orca" }));

        assert_eq!(item["id"], "call-2");
        assert_eq!(item["type"], "dynamicToolCall");
        assert!(item["namespace"].is_null());
        assert_eq!(item["tool"], "web_search");
        assert_eq!(item["status"], "in_progress");
        assert_eq!(item["arguments"]["query"], "orca");
        assert!(item["contentItems"].is_null());
        assert!(item["success"].is_null());
        assert!(item["error"].is_null());
    }

    #[test]
    fn persisted_message_thread_item_projects_existing_thread_store_shape() {
        let system = persisted_message_thread_item(ProjectedPersistedMessageThreadItem::system(
            "system context",
        ));
        let user =
            persisted_message_thread_item(ProjectedPersistedMessageThreadItem::user("hello"));
        let assistant =
            persisted_message_thread_item(ProjectedPersistedMessageThreadItem::assistant(
                Some("answer".to_string()),
                Some("reasoning".to_string()),
                vec![json!({
                    "id": "call-1",
                    "function_name": "bash",
                    "arguments": "{\"command\":\"cargo test\"}"
                })],
            ));
        let tool = persisted_message_thread_item(ProjectedPersistedMessageThreadItem::tool(
            "call-1", "ok",
        ));

        assert_eq!(
            system,
            json!({
                "role": "system",
                "content": "system context",
            })
        );
        assert_eq!(
            user,
            json!({
                "role": "user",
                "content": "hello",
            })
        );
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"], "answer");
        assert_eq!(assistant["reasoningContent"], "reasoning");
        assert_eq!(assistant["toolCalls"][0]["id"], "call-1");
        assert_eq!(
            tool,
            json!({
                "role": "tool",
                "toolCallId": "call-1",
                "content": "ok",
            })
        );
    }

    #[test]
    fn mcp_tool_completed_item_projects_success_shape() {
        let item = mcp_tool_completed_item(
            "call-3",
            "server",
            "search",
            "completed",
            json!({ "q": "orca" }),
            mcp_result_from_content(
                r#"{"content":[{"type":"text","text":"found"}],"structuredContent":{"count":1},"_meta":{"source":"test"}}"#,
            ),
            Value::Null,
        );

        assert_eq!(item["id"], "call-3");
        assert_eq!(item["type"], "mcpToolCall");
        assert_eq!(item["server"], "server");
        assert_eq!(item["tool"], "search");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["arguments"]["q"], "orca");
        assert_eq!(item["result"]["content"][0]["text"], "found");
        assert_eq!(item["result"]["structuredContent"]["count"], 1);
        assert_eq!(item["result"]["_meta"]["source"], "test");
        assert!(item["error"].is_null());
    }

    #[test]
    fn mcp_tool_completed_item_projects_failure_shape() {
        let item = mcp_tool_completed_item(
            "call-4",
            "server",
            "search",
            "failed",
            json!({ "q": "orca" }),
            Value::Null,
            tool_error_object("timeout", Some(124)),
        );

        assert_eq!(item["id"], "call-4");
        assert_eq!(item["type"], "mcpToolCall");
        assert_eq!(item["status"], "failed");
        assert_eq!(item["arguments"]["q"], "orca");
        assert!(item["result"].is_null());
        assert_eq!(item["error"]["message"], "timeout");
        assert_eq!(item["error"]["exitCode"], 124);
    }

    #[test]
    fn projected_mcp_tool_thread_item_serializes_current_wire_shapes() {
        assert_eq!(
            ProjectedMcpToolThreadItem::started(
                "call-1",
                "server",
                "search",
                json!({ "q": "orca" })
            )
            .into_value(),
            mcp_tool_started_item("call-1", "server", "search", json!({ "q": "orca" }))
        );
        assert_eq!(
            ProjectedMcpToolThreadItem::completed(ProjectedMcpToolCompletion {
                id: "call-4".to_string(),
                server: "server".to_string(),
                tool: "search".to_string(),
                status: "failed".to_string(),
                arguments: json!({ "q": "orca" }),
                result: Value::Null,
                error: tool_error_object("timeout", Some(124)),
            })
            .into_value(),
            mcp_tool_completed_item(
                "call-4",
                "server",
                "search",
                "failed",
                json!({ "q": "orca" }),
                Value::Null,
                tool_error_object("timeout", Some(124)),
            )
        );
    }

    #[test]
    fn dynamic_tool_completed_item_projects_success_shape() {
        let item = dynamic_tool_completed_item(
            "call-5",
            "deploy",
            "completed",
            json!({ "env": "staging" }),
            json!([{ "type": "text", "text": "deployed" }]),
            true,
            Value::Null,
        );

        assert_eq!(item["id"], "call-5");
        assert_eq!(item["type"], "dynamicToolCall");
        assert!(item["namespace"].is_null());
        assert_eq!(item["tool"], "deploy");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["arguments"]["env"], "staging");
        assert_eq!(item["contentItems"][0]["text"], "deployed");
        assert_eq!(item["success"], true);
        assert!(item["error"].is_null());
    }

    #[test]
    fn dynamic_tool_completed_item_projects_failure_shape() {
        let item = dynamic_tool_completed_item(
            "call-6",
            "deploy",
            "denied",
            json!({ "env": "production" }),
            Value::Null,
            false,
            tool_error_object("policy denied", None),
        );

        assert_eq!(item["id"], "call-6");
        assert_eq!(item["type"], "dynamicToolCall");
        assert_eq!(item["status"], "denied");
        assert_eq!(item["arguments"]["env"], "production");
        assert!(item["contentItems"].is_null());
        assert_eq!(item["success"], false);
        assert_eq!(item["error"]["message"], "policy denied");
    }

    #[test]
    fn projected_dynamic_tool_thread_item_serializes_current_wire_shapes() {
        assert_eq!(
            ProjectedDynamicToolThreadItem::started(
                "call-2",
                "web_search",
                json!({ "query": "orca" }),
            )
            .into_value(),
            dynamic_tool_started_item("call-2", "web_search", json!({ "query": "orca" }))
        );
        assert_eq!(
            ProjectedDynamicToolThreadItem::completed(ProjectedDynamicToolCompletion {
                id: "call-5".to_string(),
                tool: "deploy".to_string(),
                status: "completed".to_string(),
                arguments: json!({ "env": "staging" }),
                content_items: json!([{ "type": "text", "text": "deployed" }]),
                success: true,
                error: Value::Null,
            })
            .into_value(),
            dynamic_tool_completed_item(
                "call-5",
                "deploy",
                "completed",
                json!({ "env": "staging" }),
                json!([{ "type": "text", "text": "deployed" }]),
                true,
                Value::Null,
            )
        );
    }

    #[test]
    fn file_change_started_item_projects_codex_style_shape() {
        let item = file_change_started_item(
            "call-7:file-change",
            Some("src/main.rs"),
            "edit",
            Value::from(""),
        );

        assert_eq!(item["id"], "call-7:file-change");
        assert_eq!(item["type"], "fileChange");
        assert_eq!(item["status"], "inProgress");
        assert_eq!(item["changes"][0]["path"], "src/main.rs");
        assert_eq!(item["changes"][0]["kind"], "edit");
        assert_eq!(item["changes"][0]["diff"], "");
        assert!(item.get("tool").is_none());
        assert!(item.get("error").is_none());
    }

    #[test]
    fn file_change_completed_item_projects_failure_shape() {
        let item = file_change_completed_item(
            "call-8:file-change",
            None::<String>,
            "write",
            Value::from("failed"),
            Value::from("diff"),
        );

        assert_eq!(item["id"], "call-8:file-change");
        assert_eq!(item["type"], "fileChange");
        assert_eq!(item["status"], "failed");
        assert!(item["changes"][0]["path"].is_null());
        assert_eq!(item["changes"][0]["kind"], "write");
        assert_eq!(item["changes"][0]["diff"], "diff");
        assert!(item.get("tool").is_none());
        assert!(item.get("output").is_none());
    }

    #[test]
    fn projected_file_change_thread_item_serializes_current_wire_shapes() {
        assert_eq!(
            ProjectedFileChangeThreadItem::started(
                "call-7:file-change",
                Some("src/main.rs"),
                "edit",
                Value::from(""),
            )
            .into_value(),
            file_change_started_item(
                "call-7:file-change",
                Some("src/main.rs"),
                "edit",
                Value::from(""),
            )
        );
        assert_eq!(
            ProjectedFileChangeThreadItem::completed(ProjectedFileChangeCompletion {
                id: "call-8:file-change".to_string(),
                path: None,
                kind: "write".to_string(),
                status: Value::from("failed"),
                diff: Value::from("diff"),
            })
            .into_value(),
            file_change_completed_item(
                "call-8:file-change",
                None::<String>,
                "write",
                Value::from("failed"),
                Value::from("diff"),
            )
        );
    }

    #[test]
    fn projected_file_change_item_serializes_current_lifecycle_shapes() {
        let item = ProjectedFileChangeItem::new("call-7:file-change", Some("src/main.rs"), "edit");
        assert_eq!(
            item.started_item(Value::from("")),
            file_change_started_item(
                "call-7:file-change",
                Some("src/main.rs"),
                "edit",
                Value::from(""),
            )
        );

        assert_eq!(
            item.completed_item(Value::from("completed"), Value::from("diff")),
            file_change_completed_item(
                "call-7:file-change",
                Some("src/main.rs"),
                "edit",
                Value::from("completed"),
                Value::from("diff"),
            )
        );
    }

    #[test]
    fn persisted_file_change_started_item_projects_edit_history_shape() {
        let item = persisted_file_change_started_item(
            "edit-call-1",
            "edit",
            &json!({
                "path": "src/lib.rs",
                "old_text": "before",
                "new_text": "after",
            }),
        )
        .expect("edit projects as fileChange");

        assert_eq!(item["id"], "edit-call-1:file-change");
        assert_eq!(item["type"], "fileChange");
        assert_eq!(item["status"], "inProgress");
        assert_eq!(item["changes"][0]["path"], "src/lib.rs");
        assert_eq!(item["changes"][0]["kind"], "edit");
        assert_eq!(item["changes"][0]["diff"], "");
        assert!(item.get("tool").is_none());
        assert!(item.get("output").is_none());
        assert!(item.get("error").is_none());
    }

    #[test]
    fn persisted_file_change_completed_item_preserves_file_change_metadata() {
        let started = persisted_file_change_started_item(
            "write-call-1",
            "write_file",
            &json!({
                "path": "notes/new.txt",
                "content": "hello",
            }),
        )
        .expect("write_file projects as fileChange");
        let completed = persisted_file_change_completed_item(&started, Value::from("completed"));

        assert_eq!(completed["id"], "write-call-1:file-change");
        assert_eq!(completed["type"], "fileChange");
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["changes"][0]["path"], "notes/new.txt");
        assert_eq!(completed["changes"][0]["kind"], "write");
        assert_eq!(completed["changes"][0]["diff"], "");
        assert!(completed.get("tool").is_none());
        assert!(completed.get("output").is_none());
        assert!(completed.get("error").is_none());
    }

    #[test]
    fn complete_projected_tool_item_preserves_history_completion_shapes() {
        let mut command =
            persisted_command_execution_started_item("tool-6", "bash", Value::from("cargo test"));
        complete_projected_tool_item(
            &mut command,
            &json!({
                "content": "ok",
                "status": "completed",
                "truncated": true,
            }),
        );
        assert_eq!(command["type"], "commandExecution");
        assert_eq!(command["status"], "completed");
        assert_eq!(command["aggregatedOutput"], "ok");
        assert!(command["error"].is_null());
        assert_eq!(command["truncated"], true);

        let mut dynamic = dynamic_tool_started_item("call-9", "deploy", json!({ "env": "prod" }));
        complete_projected_tool_item(
            &mut dynamic,
            &json!({
                "content": "ERROR: blocked",
            }),
        );
        assert_eq!(dynamic["type"], "dynamicToolCall");
        assert_eq!(dynamic["status"], "failed");
        assert!(dynamic["contentItems"].is_null());
        assert_eq!(dynamic["success"], false);
        assert_eq!(dynamic["error"]["message"], "blocked");
    }

    #[test]
    fn workflow_started_item_projects_codex_style_shape() {
        let item = workflow_started_item(
            "workflow-run-1",
            "workflow-task-1",
            "audit",
            json!({ "kind": "workflow", "status": "running" }),
        );

        assert_eq!(item["id"], "workflow-run-1");
        assert_eq!(item["type"], "workflow");
        assert_eq!(item["workflowName"], "audit");
        assert_eq!(item["taskId"], "workflow-task-1");
        assert_eq!(item["status"], "running");
        assert_eq!(item["task"]["kind"], "workflow");
        assert!(item.get("result").is_none());
        assert!(item.get("error").is_none());
    }

    #[test]
    fn workflow_completed_item_projects_failure_shape() {
        let item = workflow_completed_item(
            "workflow-run-2",
            "workflow-task-2",
            "audit",
            "failed",
            Value::Null,
            json!({ "message": "boom" }),
            json!({ "kind": "workflow", "status": "failed" }),
        );

        assert_eq!(item["id"], "workflow-run-2");
        assert_eq!(item["type"], "workflow");
        assert_eq!(item["workflowName"], "audit");
        assert_eq!(item["taskId"], "workflow-task-2");
        assert_eq!(item["status"], "failed");
        assert!(item["result"].is_null());
        assert_eq!(item["error"]["message"], "boom");
        assert_eq!(item["task"]["status"], "failed");
    }

    #[test]
    fn projected_workflow_thread_item_serializes_current_wire_shapes() {
        assert_eq!(
            ProjectedWorkflowThreadItem::started(
                "workflow-run-3",
                "workflow-task-3",
                "audit",
                json!({ "kind": "workflow", "status": "running" }),
            )
            .into_value(),
            workflow_started_item(
                "workflow-run-3",
                "workflow-task-3",
                "audit",
                json!({ "kind": "workflow", "status": "running" }),
            )
        );
        assert_eq!(
            ProjectedWorkflowThreadItem::completed(ProjectedWorkflowCompletion {
                id: "workflow-run-4".to_string(),
                task_id: "workflow-task-4".to_string(),
                workflow_name: "audit".to_string(),
                status: "completed".to_string(),
                result: Value::from("ok"),
                error: Value::Null,
                task: json!({ "kind": "workflow", "status": "completed" }),
            })
            .into_value(),
            workflow_completed_item(
                "workflow-run-4",
                "workflow-task-4",
                "audit",
                "completed",
                Value::from("ok"),
                Value::Null,
                json!({ "kind": "workflow", "status": "completed" }),
            )
        );
    }

    #[test]
    fn projected_workflow_item_serializes_current_lifecycle_shapes() {
        let mut item = ProjectedWorkflowItem::started(
            "workflow-run-3",
            "workflow-task-3",
            "audit",
            json!({ "kind": "workflow", "status": "running" }),
        );
        assert_eq!(
            item.started_item(),
            workflow_started_item(
                "workflow-run-3",
                "workflow-task-3",
                "audit",
                json!({ "kind": "workflow", "status": "running" }),
            )
        );

        item.record_result(
            Value::from("ok"),
            json!({ "kind": "workflow", "status": "completed" }),
        );
        assert_eq!(
            item.completed_item("completed", Value::Null),
            workflow_completed_item(
                "workflow-run-3",
                "workflow-task-3",
                "audit",
                "completed",
                Value::from("ok"),
                Value::Null,
                json!({ "kind": "workflow", "status": "completed" }),
            )
        );
    }

    #[test]
    fn projected_thread_item_serializes_all_typed_transcript_shapes() {
        assert_eq!(
            ProjectedThreadItem::from(ProjectedUserMessageThreadItem::new("hello")).into_value(),
            user_message_item("hello")
        );
        assert_eq!(
            ProjectedThreadItem::from(ProjectedCommandExecutionThreadItem::started(
                "tool-1",
                "bash",
                Some("cargo test"),
            ))
            .into_value(),
            command_execution_started_item("tool-1", "bash", Some("cargo test"))
        );
        assert_eq!(
            ProjectedThreadItem::from(ProjectedMcpToolThreadItem::started(
                "call-1",
                "server",
                "search",
                json!({ "q": "orca" }),
            ))
            .into_value(),
            mcp_tool_started_item("call-1", "server", "search", json!({ "q": "orca" }))
        );
        assert_eq!(
            ProjectedThreadItem::from(ProjectedDynamicToolThreadItem::started(
                "call-2",
                "deploy",
                json!({ "env": "prod" }),
            ))
            .into_value(),
            dynamic_tool_started_item("call-2", "deploy", json!({ "env": "prod" }))
        );
        assert_eq!(
            ProjectedThreadItem::from(ProjectedFileChangeThreadItem::started(
                "call-3:file-change",
                Some("src/lib.rs"),
                "edit",
                Value::from(""),
            ))
            .into_value(),
            file_change_started_item(
                "call-3:file-change",
                Some("src/lib.rs"),
                "edit",
                Value::from(""),
            )
        );
        assert_eq!(
            ProjectedThreadItem::from(ProjectedWorkflowThreadItem::started(
                "workflow-run-5",
                "workflow-task-5",
                "audit",
                json!({ "kind": "workflow", "status": "running" }),
            ))
            .into_value(),
            workflow_started_item(
                "workflow-run-5",
                "workflow-task-5",
                "audit",
                json!({ "kind": "workflow", "status": "running" }),
            )
        );
    }

    #[test]
    fn projected_tool_call_item_serializes_current_lifecycle_shapes() {
        let command =
            ProjectedToolCallItem::command_execution("tool-1", "bash", Some("cargo test"));
        assert_eq!(
            command.started_item(Value::Null),
            command_execution_started_item("tool-1", "bash", Some("cargo test"))
        );
        assert_eq!(
            command.completed_item(ProjectedToolCallCompletion {
                status: "completed".to_string(),
                command_status: Value::from("completed"),
                arguments: Value::Null,
                result: Value::Null,
                command_error: Value::Null,
                mcp_error: Value::Null,
                dynamic_error: Value::Null,
                content_items: Value::Null,
                success: true,
                aggregated_output: Value::from("ok"),
                exit_code: Value::from(0),
                truncated: Value::from(false),
            }),
            command_execution_completed_item(
                "tool-1",
                "bash",
                Some("cargo test"),
                Value::from("completed"),
                Value::from("ok"),
                Value::Null,
                Value::from(0),
                Value::from(false),
            )
        );

        let mcp = ProjectedToolCallItem::mcp_tool("call-1", "server", "search");
        assert_eq!(
            mcp.started_item(json!({ "q": "orca" })),
            mcp_tool_started_item("call-1", "server", "search", json!({ "q": "orca" }))
        );
        assert_eq!(
            mcp.completed_item(ProjectedToolCallCompletion {
                status: "failed".to_string(),
                command_status: Value::from("failed"),
                arguments: json!({ "q": "orca" }),
                result: Value::Null,
                command_error: Value::Null,
                mcp_error: json!({ "message": "nope" }),
                dynamic_error: Value::Null,
                content_items: Value::Null,
                success: false,
                aggregated_output: Value::Null,
                exit_code: Value::Null,
                truncated: Value::Null,
            }),
            mcp_tool_completed_item(
                "call-1",
                "server",
                "search",
                "failed",
                json!({ "q": "orca" }),
                Value::Null,
                json!({ "message": "nope" }),
            )
        );

        let dynamic = ProjectedToolCallItem::dynamic_tool("call-2", "deploy");
        assert_eq!(
            dynamic.started_item(json!({ "env": "prod" })),
            dynamic_tool_started_item("call-2", "deploy", json!({ "env": "prod" }))
        );
        assert_eq!(
            dynamic.completed_item(ProjectedToolCallCompletion {
                status: "completed".to_string(),
                command_status: Value::from("completed"),
                arguments: json!({ "env": "prod" }),
                result: Value::Null,
                command_error: Value::Null,
                mcp_error: Value::Null,
                dynamic_error: Value::Null,
                content_items: json!([{ "type": "text", "text": "done" }]),
                success: true,
                aggregated_output: Value::Null,
                exit_code: Value::Null,
                truncated: Value::Null,
            }),
            dynamic_tool_completed_item(
                "call-2",
                "deploy",
                "completed",
                json!({ "env": "prod" }),
                json!([{ "type": "text", "text": "done" }]),
                true,
                Value::Null,
            )
        );
    }

    #[test]
    fn command_execution_started_item_projects_runtime_tool_shape() {
        let item = command_execution_started_item("tool-1", "bash", Some("cargo test"));

        assert_eq!(item["id"], "tool-1");
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["tool"], "bash");
        assert_eq!(item["command"], "cargo test");
        assert_eq!(item["status"], "in_progress");
        assert!(item.get("aggregatedOutput").is_none());
        assert!(item.get("exitCode").is_none());
    }

    #[test]
    fn command_execution_completed_item_projects_success_shape() {
        let item = command_execution_completed_item(
            "tool-2",
            "bash",
            Some("cargo test"),
            Value::from("completed"),
            Value::from("ok"),
            Value::Null,
            Value::from(0),
            Value::Null,
        );

        assert_eq!(item["id"], "tool-2");
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["tool"], "bash");
        assert_eq!(item["command"], "cargo test");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["aggregatedOutput"], "ok");
        assert!(item.get("output").is_none());
        assert!(item["error"].is_null());
        assert_eq!(item["exitCode"], 0);
        assert!(item["truncated"].is_null());
    }

    #[test]
    fn command_execution_completed_item_projects_failure_diagnostics() {
        let item = command_execution_completed_item(
            "tool-3",
            "bash",
            None::<String>,
            Value::from("failed"),
            Value::from("test failure details"),
            Value::from("command failed"),
            Value::from(101),
            Value::from(true),
        );

        assert_eq!(item["id"], "tool-3");
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["tool"], "bash");
        assert!(item["command"].is_null());
        assert_eq!(item["status"], "failed");
        assert_eq!(item["aggregatedOutput"], "test failure details");
        assert_eq!(item["error"], "command failed");
        assert_eq!(item["exitCode"], 101);
        assert_eq!(item["truncated"], true);
    }

    #[test]
    fn unified_exec_completion_projects_inner_process_status_and_output() {
        let mut item = persisted_command_execution_started_item(
            "tool-exec",
            "exec_command",
            Value::from("cargo test"),
        );
        let content = serde_json::json!({
            "session_id": "shell-1",
            "task_id": "task-1",
            "status": "failed",
            "termination": "exited",
            "output": "tests failed",
            "exit_code": 101,
            "truncated": true,
            "omitted_prefix_bytes": 0,
            "next_output_offset": 12,
            "output_bytes_total": 24,
            "requested_terminal": "pipe",
            "effective_terminal": "pipe"
        })
        .to_string();
        complete_projected_tool_item(
            &mut item,
            &serde_json::json!({
                "content": content,
                "status": "completed",
                "exit_code": 0,
                "truncated": false
            }),
        );

        assert_eq!(item["status"], "failed");
        assert_eq!(item["aggregatedOutput"], "tests failed");
        assert_eq!(item["error"]["message"], "tests failed");
        assert_eq!(item["exitCode"], 101);
        assert_eq!(item["truncated"], true);
    }

    #[test]
    fn projected_command_execution_thread_item_serializes_current_wire_shapes() {
        assert_eq!(
            ProjectedCommandExecutionThreadItem::started("tool-1", "bash", Some("cargo test"))
                .into_value(),
            command_execution_started_item("tool-1", "bash", Some("cargo test"))
        );
        assert_eq!(
            ProjectedCommandExecutionThreadItem::completed(ProjectedCommandExecutionCompletion {
                id: "tool-2".to_string(),
                tool: "bash".to_string(),
                command: Some("cargo test".to_string()),
                status: Value::from("completed"),
                aggregated_output: Value::from("ok"),
                error: Value::Null,
                exit_code: Value::from(0),
                truncated: Value::Null,
            })
            .into_value(),
            command_execution_completed_item(
                "tool-2",
                "bash",
                Some("cargo test"),
                Value::from("completed"),
                Value::from("ok"),
                Value::Null,
                Value::from(0),
                Value::Null,
            )
        );
    }

    #[test]
    fn persisted_command_execution_started_item_keeps_history_shape() {
        let item = persisted_command_execution_started_item("tool-4", "bash", Value::from("ls"));

        assert_eq!(item["id"], "tool-4");
        assert_eq!(item["type"], "commandExecution");
        assert_eq!(item["tool"], "bash");
        assert_eq!(item["command"], "ls");
        assert!(item["cwd"].is_null());
        assert!(item["processId"].is_null());
        assert!(item["source"].is_null());
        assert_eq!(item["status"], "in_progress");
        assert_eq!(item["commandActions"], json!([]));
        assert!(item["aggregatedOutput"].is_null());
        assert!(item["error"].is_null());
        assert!(item["exitCode"].is_null());
        assert!(item["durationMs"].is_null());
    }

    #[test]
    fn persisted_command_execution_completed_item_preserves_history_metadata() {
        let started =
            persisted_command_execution_started_item("tool-5", "bash", Value::from("cargo test"));
        let completed = persisted_command_execution_completed_item(
            &started,
            Value::from("completed"),
            Value::from("ok"),
            Value::Null,
            Value::from(true),
        );

        assert_eq!(completed["id"], "tool-5");
        assert_eq!(completed["type"], "commandExecution");
        assert_eq!(completed["tool"], "bash");
        assert_eq!(completed["command"], "cargo test");
        assert!(completed["cwd"].is_null());
        assert!(completed["processId"].is_null());
        assert!(completed["source"].is_null());
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["commandActions"], json!([]));
        assert_eq!(completed["aggregatedOutput"], "ok");
        assert!(completed["error"].is_null());
        assert!(completed["exitCode"].is_null());
        assert!(completed["durationMs"].is_null());
        assert_eq!(completed["truncated"], true);
        assert!(completed.get("result").is_none());
        assert!(completed.get("output").is_none());
    }
}
