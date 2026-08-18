use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use serde_json::{Value, json};

use orca_core::approval_types::ActionKind;
use orca_core::external_config::ExternalToolConfig;
use orca_core::mcp_types::McpTool;
use orca_core::tool_types::{
    CapabilitySet, InterruptSemantics, MAX_TOOL_OUTPUT_BYTES, RendererHint, ReplaySemantics,
    ResultSemantics, ToolCapability, ToolControlSemantics, ToolExposure, ToolName,
    ToolOutputTruncation, ToolRequest, ToolResult, ToolSpec,
};
use orca_mcp::{McpElicitationHandler, McpRegistry, McpRequestError, client::McpCallOutput};

use crate::{
    bash, edit, external, git, glob, grep, list_files, read_file, skills, update_goal, update_plan,
    web_search, write_file,
};

#[cfg(test)]
mod windows_shell_prompt_contract_tests {
    use super::*;

    #[test]
    fn bash_tool_description_does_not_claim_a_posix_shell_on_every_host() {
        let registry = default_tool_registry();
        let bash = registry.resolve("bash").expect("bash tool");

        assert!(bash.tool.description().contains("resolved host shell"));
        assert!(!bash.tool.description().contains("via sh -c"));
    }
}

#[allow(dead_code)]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    fn name(&self) -> &str {
        self.spec().name.as_str()
    }

    fn description(&self) -> &str {
        &self.spec().description
    }

    fn action_kind(&self) -> ActionKind {
        self.spec().capabilities.action_kind()
    }

    fn is_read_only(&self, _input: &ToolRequest) -> bool {
        self.spec().capabilities.is_read_only()
    }

    fn is_concurrent_safe(&self, input: &ToolRequest) -> bool;
    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult;

    fn timeout(&self) -> Duration {
        Duration::from_secs(60)
    }
}

pub struct ToolContext<'a> {
    pub cwd: &'a Path,
    pub output_truncation: ToolOutputTruncation,
    pub shell_timeout: Duration,
    pub additional_working_directories: Vec<PathBuf>,
    pub mcp_registry: Option<&'a McpRegistry>,
    pub mcp_elicitation_handler: Option<&'a dyn McpElicitationHandler>,
    pub should_cancel: Option<&'a dyn Fn() -> bool>,
}

impl<'a> ToolContext<'a> {
    pub fn new(cwd: &'a Path) -> Self {
        Self {
            cwd,
            output_truncation: ToolOutputTruncation::bytes(MAX_TOOL_OUTPUT_BYTES),
            shell_timeout: Duration::from_secs(120),
            additional_working_directories: Vec::new(),
            mcp_registry: None,
            mcp_elicitation_handler: None,
            should_cancel: None,
        }
    }

    pub fn with_output_truncation(mut self, output_truncation: ToolOutputTruncation) -> Self {
        self.output_truncation = output_truncation.normalized();
        self
    }

    pub fn with_shell_timeout(mut self, shell_timeout: Duration) -> Self {
        self.shell_timeout = shell_timeout;
        self
    }

    pub fn with_additional_working_directories(
        mut self,
        directories: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        self.additional_working_directories = directories.into_iter().collect();
        self
    }

    pub fn max_output_bytes(&self) -> usize {
        match self.output_truncation {
            ToolOutputTruncation::Bytes { limit } => limit,
            ToolOutputTruncation::Tokens { limit } => limit.saturating_mul(4),
        }
    }

    pub fn with_mcp(mut self, mcp_registry: &'a McpRegistry) -> Self {
        self.mcp_registry = Some(mcp_registry);
        self
    }

    pub fn with_mcp_elicitation_handler(mut self, handler: &'a dyn McpElicitationHandler) -> Self {
        self.mcp_elicitation_handler = Some(handler);
        self
    }

    pub fn with_cancel(mut self, should_cancel: &'a dyn Fn() -> bool) -> Self {
        self.should_cancel = Some(should_cancel);
        self
    }

    pub fn is_cancelled(&self) -> bool {
        self.should_cancel
            .is_some_and(|should_cancel| should_cancel())
    }
}

pub struct ResolvedTool<'a> {
    pub tool: &'a dyn Tool,
    pub spec: &'a ToolSpec,
    pub requested_name: ToolName,
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    by_name: HashMap<String, usize>,
    aliases: HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            by_name: HashMap::new(),
            aliases: HashMap::new(),
        }
    }

    pub fn register<T>(&mut self, tool: T)
    where
        T: Tool + 'static,
    {
        let name = tool.name().to_string();
        if self.by_name.contains_key(&name) || self.aliases.contains_key(&name) {
            return;
        }
        let idx = self.tools.len();
        self.by_name.insert(name, idx);
        for alias in &tool.spec().aliases {
            let alias = alias.as_str().to_string();
            if !self.by_name.contains_key(&alias) && !self.aliases.contains_key(&alias) {
                self.aliases.insert(alias, idx);
            }
        }
        self.tools.push(Box::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.by_name
            .get(name)
            .and_then(|idx| self.tools.get(*idx))
            .map(|tool| tool.as_ref())
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(|tool| tool.as_ref())
    }

    pub fn resolve(&self, name: &str) -> Option<ResolvedTool<'_>> {
        if let Some(idx) = self.by_name.get(name) {
            let tool = self.tools.get(*idx)?.as_ref();
            return Some(ResolvedTool {
                tool,
                spec: tool.spec(),
                requested_name: ToolName::from_str(name)?,
            });
        }
        let idx = self.aliases.get(name)?;
        let tool = self.tools.get(*idx)?.as_ref();
        Some(ResolvedTool {
            tool,
            spec: tool.spec(),
            requested_name: ToolName::from_str(name)?,
        })
    }

    pub fn control_semantics(&self, name: &ToolName) -> Option<ToolControlSemantics> {
        let resolved = self.resolve(name.as_str())?;
        Some(ToolControlSemantics {
            interrupt: resolved.spec.interrupt_semantics,
            replay: resolved.spec.replay_semantics,
        })
    }

    pub fn model_visible_tools(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools
            .iter()
            .map(|tool| tool.as_ref())
            .filter(|tool| tool.spec().exposure.is_model_visible())
    }

    pub fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        let Some(resolved) = self.resolve(request.name.as_str()) else {
            return ToolResult::failed(
                request,
                format!("unknown tool: {}", request.name.as_str()),
                None,
            );
        };
        if let Err(error) = validate_arguments(request, &resolved.spec.input_schema) {
            return ToolResult::invalid_input(
                request,
                format!("tool arguments failed schema validation: {error}"),
            );
        }
        resolved.tool.execute(request, ctx)
    }
}

pub fn validate_tool_request(registry: &ToolRegistry, request: &ToolRequest) -> Result<(), String> {
    let Some(resolved) = registry.resolve(request.name.as_str()) else {
        return Err(format!("unknown tool: {}", request.name.as_str()));
    };
    validate_arguments(request, &resolved.spec.input_schema)
}

fn validate_arguments(request: &ToolRequest, schema: &Value) -> Result<(), String> {
    let raw = request.raw_arguments.as_deref().unwrap_or("{}");
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("arguments are not valid JSON: {error}"))?;
    validate_value("$", &value, schema)
}

fn validate_value(path: &str, value: &Value, schema: &Value) -> Result<(), String> {
    validate_one_of(path, value, schema)?;
    validate_any_of(path, value, schema)?;

    if let Some(expected_type) = schema.get("type").and_then(Value::as_str) {
        match expected_type {
            "object" => validate_object(path, value, schema)?,
            "array" => validate_array(path, value, schema)?,
            "string" if !value.is_string() => {
                return Err(format!(
                    "{path}: expected string, got {}",
                    value_type(value)
                ));
            }
            "number" if !value.is_number() => {
                return Err(format!(
                    "{path}: expected number, got {}",
                    value_type(value)
                ));
            }
            "integer" if value.as_i64().is_none() && value.as_u64().is_none() => {
                return Err(format!(
                    "{path}: expected integer, got {}",
                    value_type(value)
                ));
            }
            "boolean" if !value.is_boolean() => {
                return Err(format!(
                    "{path}: expected boolean, got {}",
                    value_type(value)
                ));
            }
            _ => {}
        }
    } else if value.is_object() && has_object_keywords(schema) {
        validate_object(path, value, schema)?;
    }

    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.iter().any(|allowed| allowed == value)
    {
        let allowed = values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("{path}: expected one of [{allowed}], got {value}"));
    }

    Ok(())
}

fn has_object_keywords(schema: &Value) -> bool {
    schema.get("properties").is_some()
        || schema.get("required").is_some()
        || schema.get("additionalProperties").is_some()
}

fn validate_one_of(path: &str, value: &Value, schema: &Value) -> Result<(), String> {
    let Some(branches) = schema.get("oneOf").and_then(Value::as_array) else {
        return Ok(());
    };
    let mut matched = 0;
    let mut failures = Vec::new();
    for (idx, branch) in branches.iter().enumerate() {
        match validate_value(path, value, branch) {
            Ok(()) => matched += 1,
            Err(error) => failures.push(format!("#{idx}: {error}")),
        }
    }
    if matched == 1 {
        return Ok(());
    }
    let details = if failures.is_empty() {
        "all branches matched".to_string()
    } else {
        failures.join("; ")
    };
    Err(format!(
        "{path}: expected exactly one oneOf schema to match, matched {matched}; {details}"
    ))
}

fn validate_any_of(path: &str, value: &Value, schema: &Value) -> Result<(), String> {
    let Some(branches) = schema.get("anyOf").and_then(Value::as_array) else {
        return Ok(());
    };
    let mut failures = Vec::new();
    for (idx, branch) in branches.iter().enumerate() {
        match validate_value(path, value, branch) {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(format!("#{idx}: {error}")),
        }
    }
    Err(format!(
        "{path}: expected at least one anyOf schema to match; {}",
        failures.join("; ")
    ))
}

fn validate_object(path: &str, value: &Value, schema: &Value) -> Result<(), String> {
    let Some(object) = value.as_object() else {
        return Err(format!(
            "{path}: expected object, got {}",
            value_type(value)
        ));
    };
    let properties = schema.get("properties").and_then(Value::as_object);

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(field) {
                return Err(format!(
                    "{path}: missing required property \"{field}\" (required: {})",
                    property_name_list(required.iter().filter_map(Value::as_str))
                ));
            }
        }
    }

    if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
        && let Some(properties) = properties
    {
        for key in object.keys() {
            if !properties.contains_key(key) {
                return Err(format!(
                    "{path}: unexpected property \"{key}\" (allowed: {})",
                    property_name_list(properties.keys().map(String::as_str))
                ));
            }
        }
    }

    if let Some(properties) = properties {
        for (key, child_schema) in properties {
            if let Some(child_value) = object.get(key) {
                validate_value(&format!("{path}.{key}"), child_value, child_schema)?;
            }
        }
    }

    Ok(())
}

fn property_name_list<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut names: Vec<&str> = names.collect();
    names.sort_unstable();
    names
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_array(path: &str, value: &Value, schema: &Value) -> Result<(), String> {
    let Some(items) = value.as_array() else {
        return Err(format!("{path}: expected array, got {}", value_type(value)));
    };
    if let Some(item_schema) = schema.get("items") {
        for (idx, item) in items.iter().enumerate() {
            validate_value(&format!("{path}[{idx}]"), item, item_schema)?;
        }
    }
    Ok(())
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

static DEFAULT_REGISTRY: LazyLock<ToolRegistry> = LazyLock::new(|| {
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry);
    registry
});

pub fn default_tool_registry() -> &'static ToolRegistry {
    &DEFAULT_REGISTRY
}

pub fn tool_registry_with_mcp_and_external(
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry);
    for tool in external_tools {
        registry.register(ExternalTool::new(tool.clone()));
    }
    if let Some(mcp_registry) = mcp_registry {
        for tool in mcp_registry.tools() {
            registry.register(McpProxyTool::new(tool.clone()));
        }
    }
    registry
}

fn register_builtin_tools(registry: &mut ToolRegistry) {
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "read_file",
            "Read the contents of a file at the given path relative to workspace root.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based line number to start reading from. Use for large files."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Number of lines to read. Use with offset for large files."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            CapabilitySet::read_only_fs(),
            ToolExposure::Direct,
            RendererHint::FileRead,
            true,
        ),
        BuiltinExecutor::ReadFile,
    ));
    let mut glob = safe_local_read_builtin_spec(
        "glob",
        "Find files and directories by glob pattern or fuzzy path query. Use this for project file discovery.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "description": "Glob pattern such as **/*.rs"
                },
                "query": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "description": "Fuzzy path query used when mode is fuzzy, such as rcm for runtime/config/mod.rs"
                },
                "mode": {
                    "anyOf": [
                        {"type": "string", "enum": ["glob", "fuzzy"]},
                        {"type": "null"}
                    ],
                    "description": "Search mode. Defaults to glob."
                },
                "path": {
                    "anyOf": [{"type": "string"}, {"type": "null"}],
                    "description": "Directory to search in (default: '.')"
                },
                "head_limit": {
                    "anyOf": [
                        {"type": "integer", "minimum": 0},
                        {"type": "null"}
                    ],
                    "description": "Maximum number of paths to return. Defaults to 500. Use 0 for unlimited."
                },
                "offset": {
                    "anyOf": [
                        {"type": "integer", "minimum": 0},
                        {"type": "null"}
                    ],
                    "description": "Zero-based result offset for pagination."
                }
            },
            "anyOf": [
                {
                    "required": ["pattern"],
                    "properties": {
                        "mode": { "enum": ["glob"] }
                    }
                },
                {
                    "required": ["mode", "query"],
                    "properties": {
                        "mode": { "enum": ["fuzzy"] }
                    }
                }
            ],
            "additionalProperties": false
        }),
        CapabilitySet::new(vec![ToolCapability::FsList, ToolCapability::FsSearch]),
        ToolExposure::Direct,
        RendererHint::FileSearch,
        true,
    );
    glob.aliases.push(ToolName::plain("list_files"));
    registry.register(BuiltinTool::new(glob, BuiltinExecutor::Glob));
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "grep",
            "Search for a regex pattern in files using ripgrep when available, with a native in-process fallback. Returns matching lines with line numbers.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search in (default: '.')"
                    },
                    "head_limit": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Maximum number of matches to return. Defaults to 250. Use 0 for unlimited."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Zero-based result offset for pagination."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::FsSearch]),
            ToolExposure::Direct,
            RendererHint::FileSearch,
            true,
        ),
        BuiltinExecutor::Grep,
    ));
    registry.register(BuiltinTool::new(
        cooperative_builtin_spec(
            "bash",
            "Execute a command in the resolved host shell. Use for running tests, builds, git operations, etc.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    }
                },
                "required": ["command"]
            }),
            CapabilitySet::shell_execute(),
            ToolExposure::Direct,
            RendererHint::Shell,
            false,
        ),
        BuiltinExecutor::Bash,
    ));
    registry.register(BuiltinTool::new(
        cooperative_builtin_spec(
            "exec_command",
            "Start a command in the runtime-owned terminal service. Returns a session_id when the command is still running so it can be continued with write_stdin.",
            json!({
                "type": "object",
                "properties": {
                    "cmd": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Optional working directory. Relative paths are resolved from the current workspace directory."
                    },
                    "tty": {
                        "type": "boolean",
                        "description": "Allocate a PTY for interactive programs such as vim, REPLs, and terminal UIs. Defaults to false."
                    },
                    "yield_time_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 30000,
                        "description": "How long to wait before returning. Defaults to 10000ms. A running command is not terminated when this elapses."
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20000,
                        "description": "Maximum output returned by this call, measured approximately in model tokens. Defaults to 2000."
                    }
                },
                "required": ["cmd"],
                "additionalProperties": false
            }),
            CapabilitySet::shell_execute(),
            ToolExposure::Direct,
            RendererHint::Shell,
            false,
        ),
        BuiltinExecutor::ExecCommand,
    ));
    registry.register(BuiltinTool::new(
        cooperative_builtin_spec(
            "write_stdin",
            "Write characters to a running exec_command session, or poll it by omitting chars. Control characters such as Ctrl-U may be sent with their Unicode escape.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The session_id returned by exec_command"
                    },
                    "chars": {
                        "type": "string",
                        "description": "Characters to write. Omit to poll without writing."
                    },
                    "yield_time_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 30000,
                        "description": "How long to wait for output after writing or polling. Defaults to 250ms when writing and 5000ms when polling."
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20000,
                        "description": "Maximum output returned by this call, measured approximately in model tokens. Defaults to 2000."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::TerminalTransport]),
            ToolExposure::Direct,
            RendererHint::Shell,
            false,
        ),
        BuiltinExecutor::WriteStdin,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "edit",
            "Edit a file by replacing exact text. The old_text must match exactly one location in the file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact text to find (must match uniquely in the file)"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text"
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Write,
            false,
        ),
        BuiltinExecutor::Edit,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "write_file",
            "Create or overwrite a file with the given content. Use for creating new files or completely replacing file contents.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root"
                    },
                    "content": {
                        "type": "string",
                        "description": "The full content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Write,
            false,
        ),
        BuiltinExecutor::WriteFile,
    ));
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "git_status",
            "Show the git working tree status in short format.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            CapabilitySet::new(vec![ToolCapability::GitInspect]),
            ToolExposure::Direct,
            RendererHint::State,
            true,
        ),
        BuiltinExecutor::GitStatus,
    ));
    registry.register(BuiltinTool::new(
        cooperative_builtin_spec(
            "web_search",
            "Search the web for current information using Brave Search. Returns top results with title, summary, and URL.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of results to return, 1-10 (default: 5)"
                    },
                    "freshness": {
                        "type": "string",
                        "description": "Optional recency filter. Use pd for last 24 hours, pw for last 7 days, pm for last 31 days, py for last year, or YYYY-MM-DDtoYYYY-MM-DD."
                    }
                },
                "required": ["query"]
            }),
            CapabilitySet::network_search(),
            ToolExposure::Direct,
            RendererHint::Network,
            false,
        ),
        BuiltinExecutor::WebSearch,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "subagent",
            "Launch a synchronous child agent for a complex, multi-step subtask. The child runs independently and returns a concise result summary.",
            json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "Short 3-8 word label for the delegated task"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Full standalone instructions for the child agent"
                    },
                    "subagent_type": {
                        "type": "string",
                        "enum": ["general", "code_reviewer", "test_writer", "debugger", "documenter"],
                        "description": "Optional specialized agent type that restricts tools and provides focused expertise"
                    },
                    "model": {
                        "type": "string",
                        "enum": ["auto", "deepseek-v4-flash", "deepseek-v4-pro"],
                        "description": "Optional model override for this child agent. auto uses Orca's router, flash is faster, pro is stronger for deep reasoning."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["sync", "async"],
                        "description": "sync blocks until completion. Use async for long-running delegated work; it launches the child in the background, makes progress visible in task_list, and returns an agent_id for subagent_status."
                    },
                    "isolation": {
                        "type": "string",
                        "enum": ["none", "worktree"],
                        "description": "none uses the current checkout. worktree runs the child in a detached git worktree and preserves it if the child leaves file changes."
                    },
                    "schema": {
                        "type": "object",
                        "description": "Optional JSON Schema subset for validating the child agent's final output. Supports type, required, and properties."
                    }
                },
                "required": ["description", "prompt"]
            }),
            CapabilitySet::agent_delegate(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::Subagent,
    ));
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "subagent_status",
            "Query the status and result of an async subagent by agent_id. Text results are paged so large durable worker results can be recovered with offset and limit.",
            json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The agent_id returned by subagent with mode async"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional character offset into text output. Use output_next_offset from the previous response to continue reading."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 32000,
                        "description": "Optional maximum number of output characters to return. Defaults to 12000."
                    }
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::TaskRead]),
            ToolExposure::Direct,
            RendererHint::Agent,
            true,
        ),
        BuiltinExecutor::SubagentStatus,
    ));
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "task_list",
            "List background tasks in the current session, including shell, workflow, and subagent work.",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::TaskRead]),
            ToolExposure::Direct,
            RendererHint::Agent,
            true,
        ),
        BuiltinExecutor::TaskList,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "task_stop",
            "Request that a running background task stop. Prefer task_id; shell_id is accepted as a deprecated compatibility alias.",
            json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "Task id returned by task_list or shell/start"
                    },
                    "shell_id": {
                        "type": "string",
                        "description": "Deprecated alias accepted for package 3 compatibility"
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::TaskControl]),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::TaskStop,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "WorkflowDraft",
            "Create a previewable dynamic workflow draft from a JavaScript workflow script without launching it. Use this before Workflow when the user should review phases and raw script first.",
            json!({
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "Self-contained workflow script to store as a preview draft. The draft is not executed until launched through Workflow with draftId."
                    }
                },
                "required": ["script"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::WorkflowDraft,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "WorkflowDraftAction",
            "Apply a preview decision to a workflow draft. Use run to launch an approved draft, edit to replace the draft script and re-render metadata, save to persist it as a reusable workflow command, or cancel to discard it.",
            json!({
                "type": "object",
                "properties": {
                    "draftId": {
                        "type": "string",
                        "description": "Identifier returned by WorkflowDraft."
                    },
                    "action": {
                        "type": "string",
                        "enum": ["run", "edit", "save", "cancel"],
                        "description": "Decision to apply to the draft preview."
                    },
                    "script": {
                        "type": "string",
                        "description": "Replacement JavaScript workflow script for action=edit."
                    },
                    "saveAs": {
                        "type": "string",
                        "description": "Reusable workflow name for action=save. Defaults to the draft meta name."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["project", "user"],
                        "description": "Where to save reusable workflows. Project writes .orca/workflows; user writes ~/.orca/workflows."
                    },
                    "args": {
                        "type": "object",
                        "description": "Structured args passed when action=run."
                    },
                    "tokenBudget": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional run-level token budget passed to the workflow."
                    }
                },
                "required": ["draftId", "action"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::WorkflowDraftAction,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "Workflow",
            "Launch a dynamic workflow: a JavaScript script that orchestrates many subagents in the background. The tool returns task metadata immediately; the final report is delivered later as a task notification.",
            json!({
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "Self-contained workflow script. Auto mode uses export const meta = { name, description, phases: [{ name, tasks: [{ prompt: \"...\" }] }] }; every task must be an object with prompt. Hand-written mode uses string phase names plus top-level phase()/agent() calls and export default. Do not export `run` or helper functions; only export const meta, phases, args, or default."
                    },
                    "name": {
                        "type": "string",
                        "description": "Name of a predefined workflow from .orca/workflows/ or the user workflow directory."
                    },
                    "description": {
                        "type": "string",
                        "description": "Compatibility field ignored by the runtime; use meta.description in the script."
                    },
                    "title": {
                        "type": "string",
                        "description": "Compatibility field ignored by the runtime; use meta.name in the script."
                    },
                    "args": {
                        "type": "object",
                        "description": "Structured input exposed to the workflow script as the global args value."
                    },
                    "tokenBudget": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional run-level token budget. Each child reserves available capacity before it starts, settled usage is reconciled after completion, and the result reports total, spent, and remaining."
                    },
                    "draftId": {
                        "type": "string",
                        "description": "Identifier of a previewed workflow draft to launch."
                    },
                    "scriptPath": {
                        "type": "string",
                        "description": "Path to a workflow script file. Takes precedence over script and name."
                    },
                    "resumeFromRunId": {
                        "type": "string",
                        "description": "Run id of a prior same-session workflow invocation to resume from."
                    }
                },
                "required": []
            }),
            CapabilitySet::new(vec![ToolCapability::WorkflowRun]),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::Workflow,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "workflow_send_message",
            "Send a message to the current workflow run mailbox so later workflow child agents can read it.",
            json!({
                "type": "object",
                "properties": {
                    "channel": {
                        "type": "string",
                        "description": "Mailbox channel name"
                    },
                    "message": {
                        "description": "JSON-serializable message payload"
                    },
                    "from": {
                        "type": "string",
                        "description": "Optional sender label"
                    }
                },
                "required": ["channel", "message"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::WorkflowSendMessage,
    ));
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "workflow_read_messages",
            "Read messages from a channel in the current workflow run mailbox.",
            json!({
                "type": "object",
                "properties": {
                    "channel": {
                        "type": "string",
                        "description": "Mailbox channel name"
                    }
                },
                "required": ["channel"]
            }),
            CapabilitySet::read_only_fs(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::WorkflowReadMessages,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "workflow_clear_messages",
            "Clear messages from a channel in the current workflow run mailbox.",
            json!({
                "type": "object",
                "properties": {
                    "channel": {
                        "type": "string",
                        "description": "Mailbox channel name"
                    }
                },
                "required": ["channel"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::WorkflowClearMessages,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "workflow_create_task_list",
            "Create or replace a task list in the current workflow run so workflow child agents can claim shared work.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Task list name"
                    },
                    "items": {
                        "type": "array",
                        "description": "Task payloads to enqueue",
                        "items": {}
                    }
                },
                "required": ["name", "items"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::WorkflowCreateTaskList,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "workflow_claim_task",
            "Claim the next pending task from a task list in the current workflow run.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Task list name"
                    },
                    "by": {
                        "type": "string",
                        "description": "Optional worker label"
                    }
                },
                "required": ["name"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::WorkflowClaimTask,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "workflow_complete_task",
            "Mark a claimed workflow task as completed with a JSON-serializable result.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Task list name"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Task id returned by workflow_claim_task or workflow_list_tasks"
                    },
                    "result": {
                        "description": "JSON-serializable task result"
                    },
                    "by": {
                        "type": "string",
                        "description": "Optional worker label"
                    }
                },
                "required": ["name", "task_id"]
            }),
            CapabilitySet::filesystem_write(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::WorkflowCompleteTask,
    ));
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "workflow_list_tasks",
            "List tasks in a task list from the current workflow run.",
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Task list name"
                    }
                },
                "required": ["name"]
            }),
            CapabilitySet::read_only_fs(),
            ToolExposure::Direct,
            RendererHint::Agent,
            false,
        ),
        BuiltinExecutor::WorkflowListTasks,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "update_plan",
            "Update the current task plan. Use for complex multi-step tasks or when the user asks for a todo/task list. At most one step may be in_progress. Maximum 50 items, each step max 200 chars.",
            json!({
                "type": "object",
                "properties": {
                    "explanation": {
                        "anyOf": [{"type": "string"}, {"type": "null"}],
                        "description": "Optional short explanation for this plan update"
                    },
                    "plan": {
                        "type": "array",
                        "description": "The complete current list of task steps",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {
                                    "type": "string",
                                    "description": "Task step text"
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Step status"
                                }
                            },
                            "required": ["step", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::PlanUpdate]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::UpdatePlan,
    ));
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "get_goal",
            "Read the active persistent goal for the current goal-mode session, including objective, status, usage, and budget.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::GoalUpdate]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::GetGoal,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "create_goal",
            "Create a new active persistent goal for the current goal-mode session. This cannot replace an unfinished goal; complete or block the existing goal first.",
            json!({
                "type": "object",
                "properties": {
                    "objective": {
                        "type": "string",
                        "description": "User-facing goal objective to pursue"
                    },
                    "token_budget": {
                        "type": "integer",
                        "description": "Optional positive token budget for this goal"
                    }
                },
                "required": ["objective"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::GoalUpdate]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::CreateGoal,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "update_goal",
            "Submit a terminal intent for the active persistent goal. The runtime audits complete and blocked claims at outer-turn end; a successful tool call does not itself make the Goal terminal. Pause, resume, clear, budget, and objective edits remain user or system controls.",
            json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["blocked", "complete"],
                        "description": "Terminal model-controlled goal status"
                    },
                    "reason": {
                        "anyOf": [{"type": "string"}, {"type": "null"}],
                        "description": "Optional short reason for the status update"
                    },
                    "evidence": {
                        "type": "array",
                        "description": "Concrete evidence supporting the terminal claim",
                        "maxItems": 32,
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": {
                                    "type": "string",
                                    "enum": ["test", "file", "command", "observation", "external"]
                                },
                                "summary": {"type": "string"},
                                "target": {
                                    "anyOf": [{"type": "string"}, {"type": "null"}]
                                }
                            },
                            "required": ["kind", "summary"],
                            "additionalProperties": false
                        }
                    },
                    "blocker": {
                        "anyOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "enum": [
                                            "user_decision",
                                            "missing_authority",
                                            "external_state",
                                            "environment_contradiction",
                                            "unverifiable_requirement"
                                        ]
                                    },
                                    "summary": {"type": "string"}
                                },
                                "required": ["kind", "summary"],
                                "additionalProperties": false
                            },
                            {"type": "null"}
                        ],
                        "description": "Typed non-model-fixable dependency for a blocked intent"
                    }
                },
                "required": ["status"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::GoalUpdate]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::UpdateGoal,
    ));
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "list_skills",
            "List available user and project skills. Use before read_skill when the user asks for a skill or reusable procedure.",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::SkillRead]),
            ToolExposure::Direct,
            RendererHint::State,
            true,
        ),
        BuiltinExecutor::ListSkills,
    ));
    registry.register(BuiltinTool::new(
        safe_local_read_builtin_spec(
            "read_skill",
            "Read a skill's Markdown instructions by id after list_skills shows it is available.",
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Skill id, usually the skill directory name"
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::SkillRead]),
            ToolExposure::Direct,
            RendererHint::State,
            true,
        ),
        BuiltinExecutor::ReadSkill,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "list_mcp_resources",
            "List resources exposed by connected MCP servers. Prefer this before read_mcp_resource when the user asks for MCP-provided context.",
            json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "Optional MCP server name to filter resources by"
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::McpResourceRead]),
            ToolExposure::Direct,
            RendererHint::State,
            true,
        ),
        BuiltinExecutor::ListMcpResources,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "list_mcp_resource_templates",
            "List URI templates exposed by connected MCP servers for parameterized resources.",
            json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "Optional MCP server name to filter resource templates by"
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::McpResourceRead]),
            ToolExposure::Direct,
            RendererHint::State,
            true,
        ),
        BuiltinExecutor::ListMcpResourceTemplates,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "read_mcp_resource",
            "Read a specific MCP resource by server name and resource URI.",
            json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "MCP server name"
                    },
                    "uri": {
                        "type": "string",
                        "description": "Resource URI returned by list_mcp_resources"
                    }
                },
                "required": ["server", "uri"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::McpResourceRead]),
            ToolExposure::Direct,
            RendererHint::State,
            true,
        ),
        BuiltinExecutor::ReadMcpResource,
    ));
    registry.register(BuiltinTool::new(
        conservative_builtin_spec(
            "request_permissions",
            "Request additional permissions for the current turn. Compatible with Codex permission profiles; granted fileSystem.write roots are temporary and do not persist to thread metadata.",
            json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "Why the additional permission is required"
                    },
                    "permissions": {
                        "type": "object",
                        "properties": {
                            "fileSystem": {
                                "type": ["object", "null"],
                                "properties": {
                                    "read": {
                                        "type": ["array", "null"],
                                        "items": { "type": "string" }
                                    },
                                    "write": {
                                        "type": ["array", "null"],
                                        "items": { "type": "string" }
                                    },
                                    "globScanMaxDepth": {
                                        "type": "integer"
                                    },
                                    "entries": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "path": { "type": "string" },
                                                "access": {
                                                    "type": "string",
                                                    "enum": ["read", "write", "readWrite"]
                                                }
                                            },
                                            "required": ["path", "access"],
                                            "additionalProperties": false
                                        }
                                    }
                                },
                                "additionalProperties": false
                            },
                            "network": {
                                "type": ["object", "null"],
                                "properties": {
                                    "enabled": { "type": ["boolean", "null"] },
                                    "domains": {
                                        "type": "object",
                                        "additionalProperties": {
                                            "type": "string",
                                            "enum": ["allow", "deny"]
                                        }
                                    }
                                },
                                "additionalProperties": false
                            }
                        },
                        "additionalProperties": false
                    }
                },
                "required": ["reason", "permissions"],
                "additionalProperties": false
            }),
            CapabilitySet::new(vec![ToolCapability::PermissionRequest]),
            ToolExposure::Direct,
            RendererHint::State,
            false,
        ),
        BuiltinExecutor::RequestPermissions,
    ));
    let ask_user_question = conservative_builtin_spec(
        "ask_user_question",
        "Ask the user one to four structured questions when progress requires clarification. Each question offers two to four choices, while the user may also type a custom answer. Headless runs fail deterministically instead of blocking.",
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to ask the user (1-4 questions)",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "header": {
                                "type": "string",
                                "description": "Very short label for the question (maximum 12 characters)"
                            },
                            "question": {
                                "type": "string",
                                "description": "The complete, clear question to ask the user"
                            },
                            "options": {
                                "type": "array",
                                "description": "Available choices (2-4 options). Do not add an Other option; the user can type a custom answer.",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "Concise option label (1-5 words)"
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "What selecting this option means or implies"
                                        },
                                        "preview": {
                                            "type": "string",
                                            "description": "Optional preview content shown for comparison"
                                        }
                                    },
                                    "required": ["label", "description"],
                                    "additionalProperties": false
                                }
                            },
                            "multiSelect": {
                                "type": "boolean",
                                "description": "Whether the user may choose more than one option"
                            }
                        },
                        "required": ["header", "question", "options"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
        CapabilitySet::new(vec![ToolCapability::UserInputRequest]),
        ToolExposure::Direct,
        RendererHint::State,
        false,
    );
    registry.register(BuiltinTool::new(
        ask_user_question,
        BuiltinExecutor::AskUserQuestion,
    ));
}

const SAFE_LOCAL_READ: ToolControlSemantics = ToolControlSemantics {
    interrupt: InterruptSemantics::WaitForTerminal,
    replay: ReplaySemantics::SafeToRetry,
};

const CONSERVATIVE_WAIT: ToolControlSemantics = ToolControlSemantics {
    interrupt: InterruptSemantics::WaitForTerminal,
    replay: ReplaySemantics::IndeterminateAfterStart,
};

const COOPERATIVE_INDETERMINATE: ToolControlSemantics = ToolControlSemantics {
    interrupt: InterruptSemantics::CooperativeCancel,
    replay: ReplaySemantics::IndeterminateAfterStart,
};

fn safe_local_read_builtin_spec(
    name: &str,
    description: &str,
    input_schema: Value,
    capabilities: CapabilitySet,
    exposure: ToolExposure,
    renderer: RendererHint,
    concurrent_safe: bool,
) -> ToolSpec {
    builtin_spec(
        name,
        description,
        input_schema,
        capabilities,
        exposure,
        renderer,
        concurrent_safe,
        SAFE_LOCAL_READ,
    )
}

fn conservative_builtin_spec(
    name: &str,
    description: &str,
    input_schema: Value,
    capabilities: CapabilitySet,
    exposure: ToolExposure,
    renderer: RendererHint,
    concurrent_safe: bool,
) -> ToolSpec {
    builtin_spec(
        name,
        description,
        input_schema,
        capabilities,
        exposure,
        renderer,
        concurrent_safe,
        CONSERVATIVE_WAIT,
    )
}

fn cooperative_builtin_spec(
    name: &str,
    description: &str,
    input_schema: Value,
    capabilities: CapabilitySet,
    exposure: ToolExposure,
    renderer: RendererHint,
    concurrent_safe: bool,
) -> ToolSpec {
    builtin_spec(
        name,
        description,
        input_schema,
        capabilities,
        exposure,
        renderer,
        concurrent_safe,
        COOPERATIVE_INDETERMINATE,
    )
}

fn builtin_spec(
    name: &str,
    description: &str,
    input_schema: Value,
    capabilities: CapabilitySet,
    exposure: ToolExposure,
    renderer: RendererHint,
    concurrent_safe: bool,
    control: ToolControlSemantics,
) -> ToolSpec {
    ToolSpec {
        name: ToolName::plain(name),
        aliases: Vec::new(),
        description: description.to_string(),
        input_schema,
        output_schema: None,
        capabilities,
        exposure,
        result_semantics: ResultSemantics::Standard,
        interrupt_semantics: control.interrupt,
        replay_semantics: control.replay,
        renderer,
        concurrent_safe,
    }
}

fn capability_set_for_action_kind(action_kind: ActionKind) -> CapabilitySet {
    match action_kind {
        ActionKind::Read => CapabilitySet::read_only_fs(),
        ActionKind::Write => CapabilitySet::filesystem_write(),
        ActionKind::Network => CapabilitySet::network_search(),
        ActionKind::Agent => CapabilitySet::agent_delegate(),
        ActionKind::Shell => CapabilitySet::shell_execute(),
    }
}

fn renderer_for_action_kind(action_kind: ActionKind) -> RendererHint {
    match action_kind {
        ActionKind::Read => RendererHint::FileRead,
        ActionKind::Write => RendererHint::Write,
        ActionKind::Network => RendererHint::Network,
        ActionKind::Agent => RendererHint::Agent,
        ActionKind::Shell => RendererHint::Shell,
    }
}

struct BuiltinTool {
    spec: ToolSpec,
    executor: BuiltinExecutor,
}

impl BuiltinTool {
    fn new(spec: ToolSpec, executor: BuiltinExecutor) -> Self {
        Self { spec, executor }
    }
}

impl Tool for BuiltinTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn action_kind(&self) -> ActionKind {
        self.spec.capabilities.action_kind()
    }

    fn is_read_only(&self, _input: &ToolRequest) -> bool {
        self.spec.capabilities.is_read_only()
    }

    fn is_concurrent_safe(&self, input: &ToolRequest) -> bool {
        self.is_read_only(input) && self.spec.concurrent_safe
    }

    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        match self.executor {
            BuiltinExecutor::ReadFile => {
                read_file::execute_or_cancel(request, ctx.cwd, ctx.max_output_bytes(), || {
                    ctx.is_cancelled()
                })
            }
            BuiltinExecutor::Glob if request.name.as_str() == "list_files" => {
                list_files::execute(request, ctx.cwd, ctx.max_output_bytes())
            }
            BuiltinExecutor::Glob => glob::execute(request, ctx.cwd, ctx.max_output_bytes()),
            BuiltinExecutor::Grep => grep::execute(request, ctx.cwd, ctx.max_output_bytes()),
            BuiltinExecutor::Bash => bash::execute_with_policy_roots_or_cancel(
                request,
                ctx.cwd,
                &ctx.additional_working_directories,
                ctx.output_truncation,
                ctx.shell_timeout,
                || ctx.is_cancelled(),
            ),
            BuiltinExecutor::ExecCommand => ToolResult::failed(
                request,
                "exec_command must be executed by the runtime terminal service",
                None,
            ),
            BuiltinExecutor::WriteStdin => ToolResult::failed(
                request,
                "write_stdin must be executed by the runtime terminal service",
                None,
            ),
            BuiltinExecutor::Edit => {
                edit::execute_or_cancel(request, ctx.cwd, || ctx.is_cancelled())
            }
            BuiltinExecutor::WriteFile => write_file::execute(request, ctx.cwd),
            BuiltinExecutor::GitStatus => git::status(request, ctx.cwd, ctx.max_output_bytes()),
            BuiltinExecutor::WebSearch => {
                web_search::execute_or_cancel(request, ctx.max_output_bytes(), || {
                    ctx.is_cancelled()
                })
            }
            BuiltinExecutor::Subagent => ToolResult::failed(
                request,
                "subagent tool must be executed by the runtime",
                None,
            ),
            BuiltinExecutor::SubagentStatus => ToolResult::failed(
                request,
                "subagent_status tool must be executed by the runtime",
                None,
            ),
            BuiltinExecutor::TaskList => ToolResult::failed(
                request,
                "task_list tool must be executed by the runtime",
                None,
            ),
            BuiltinExecutor::TaskStop => ToolResult::failed(
                request,
                "task_stop tool must be executed by the runtime",
                None,
            ),
            BuiltinExecutor::WorkflowDraft => ToolResult::failed(
                request,
                "WorkflowDraft must be executed by the runtime controller",
                None,
            ),
            BuiltinExecutor::WorkflowDraftAction => ToolResult::failed(
                request,
                "WorkflowDraftAction must be executed by the runtime controller",
                None,
            ),
            BuiltinExecutor::Workflow => ToolResult::failed(
                request,
                "Workflow must be executed by the runtime controller",
                None,
            ),
            BuiltinExecutor::WorkflowSendMessage
            | BuiltinExecutor::WorkflowReadMessages
            | BuiltinExecutor::WorkflowClearMessages
            | BuiltinExecutor::WorkflowCreateTaskList
            | BuiltinExecutor::WorkflowClaimTask
            | BuiltinExecutor::WorkflowCompleteTask
            | BuiltinExecutor::WorkflowListTasks => ToolResult::failed(
                request,
                "workflow IPC tools must be executed by the runtime",
                None,
            ),
            BuiltinExecutor::GetGoal => update_goal::execute_get(request),
            BuiltinExecutor::CreateGoal => update_goal::execute_create(request),
            BuiltinExecutor::UpdateGoal => update_goal::execute_update(request),
            BuiltinExecutor::UpdatePlan => update_plan::execute(request),
            BuiltinExecutor::ListSkills => skills::execute_list(request, ctx.cwd),
            BuiltinExecutor::ReadSkill => skills::execute_read(request, ctx.cwd),
            BuiltinExecutor::ListMcpResources => execute_list_mcp_resources(request, ctx),
            BuiltinExecutor::ListMcpResourceTemplates => {
                execute_list_mcp_resource_templates(request, ctx)
            }
            BuiltinExecutor::ReadMcpResource => execute_read_mcp_resource(request, ctx),
            BuiltinExecutor::RequestPermissions => ToolResult::failed(
                request,
                "request_permissions must be executed by the runtime",
                None,
            ),
            BuiltinExecutor::AskUserQuestion => ToolResult::failed(
                request,
                "ask_user_question requires an interactive TUI session",
                None,
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum BuiltinExecutor {
    ReadFile,
    Glob,
    Grep,
    Bash,
    ExecCommand,
    WriteStdin,
    Edit,
    WriteFile,
    GitStatus,
    WebSearch,
    Subagent,
    SubagentStatus,
    TaskList,
    TaskStop,
    WorkflowDraft,
    WorkflowDraftAction,
    Workflow,
    WorkflowSendMessage,
    WorkflowReadMessages,
    WorkflowClearMessages,
    WorkflowCreateTaskList,
    WorkflowClaimTask,
    WorkflowCompleteTask,
    WorkflowListTasks,
    GetGoal,
    CreateGoal,
    UpdateGoal,
    UpdatePlan,
    ListSkills,
    ReadSkill,
    ListMcpResources,
    ListMcpResourceTemplates,
    ReadMcpResource,
    RequestPermissions,
    AskUserQuestion,
}

fn execute_list_mcp_resources(request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
    let Some(registry) = ctx.mcp_registry else {
        return ToolResult::failed(request, "MCP registry is not initialized", None);
    };
    let args = match parse_json_arguments(request) {
        Ok(value) => value,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let server = args.get("server").and_then(Value::as_str);
    let should_cancel = || ctx.is_cancelled();

    if server.is_none() {
        let listing = if ctx.should_cancel.is_some() {
            registry.list_resources_with_errors_or_cancel(None, &should_cancel)
        } else {
            Ok(registry.list_resources_with_errors(None))
        };
        let listing = match listing {
            Ok(listing) => listing,
            Err(McpRequestError::Cancelled) => {
                return ToolResult::cancelled(request, MCP_TOOL_CALL_CANCELLED, None);
            }
            Err(McpRequestError::Failed(error)) => {
                return ToolResult::failed(request, error, None);
            }
        };
        let output = json!({
            "resources": listing.resources,
            "errors": listing.errors,
        });
        return match serde_json::to_string(&output) {
            Ok(output) => ToolResult::completed(request, output, false),
            Err(error) => ToolResult::failed(
                request,
                format!("failed to serialize MCP resources: {error}"),
                None,
            ),
        };
    }

    let result = if ctx.should_cancel.is_some() {
        registry.list_resources_or_cancel(server, &should_cancel)
    } else {
        registry
            .list_resources(server)
            .map_err(McpRequestError::Failed)
    };
    match result {
        Ok(resources) => match serde_json::to_string(&resources) {
            Ok(output) => ToolResult::completed(request, output, false),
            Err(error) => ToolResult::failed(
                request,
                format!("failed to serialize MCP resources: {error}"),
                None,
            ),
        },
        Err(McpRequestError::Cancelled) => {
            ToolResult::cancelled(request, MCP_TOOL_CALL_CANCELLED, None)
        }
        Err(McpRequestError::Failed(error)) => ToolResult::failed(request, error, None),
    }
}

fn execute_list_mcp_resource_templates(request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
    let Some(registry) = ctx.mcp_registry else {
        return ToolResult::failed(request, "MCP registry is not initialized", None);
    };
    let args = match parse_json_arguments(request) {
        Ok(value) => value,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let server = args.get("server").and_then(Value::as_str);
    let should_cancel = || ctx.is_cancelled();

    if server.is_none() {
        let listing = if ctx.should_cancel.is_some() {
            registry.list_resource_templates_with_errors_or_cancel(None, &should_cancel)
        } else {
            Ok(registry.list_resource_templates_with_errors(None))
        };
        let listing = match listing {
            Ok(listing) => listing,
            Err(McpRequestError::Cancelled) => {
                return ToolResult::cancelled(request, MCP_TOOL_CALL_CANCELLED, None);
            }
            Err(McpRequestError::Failed(error)) => {
                return ToolResult::failed(request, error, None);
            }
        };
        let output = json!({
            "resourceTemplates": listing.resource_templates,
            "errors": listing.errors,
        });
        return match serde_json::to_string(&output) {
            Ok(output) => ToolResult::completed(request, output, false),
            Err(error) => ToolResult::failed(
                request,
                format!("failed to serialize MCP resource templates: {error}"),
                None,
            ),
        };
    }

    let result = if ctx.should_cancel.is_some() {
        registry.list_resource_templates_or_cancel(server, &should_cancel)
    } else {
        registry
            .list_resource_templates(server)
            .map_err(McpRequestError::Failed)
    };
    match result {
        Ok(resource_templates) => match serde_json::to_string(&resource_templates) {
            Ok(output) => ToolResult::completed(request, output, false),
            Err(error) => ToolResult::failed(
                request,
                format!("failed to serialize MCP resource templates: {error}"),
                None,
            ),
        },
        Err(McpRequestError::Cancelled) => {
            ToolResult::cancelled(request, MCP_TOOL_CALL_CANCELLED, None)
        }
        Err(McpRequestError::Failed(error)) => ToolResult::failed(request, error, None),
    }
}

fn execute_read_mcp_resource(request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
    let Some(registry) = ctx.mcp_registry else {
        return ToolResult::failed(request, "MCP registry is not initialized", None);
    };
    let args = match parse_json_arguments(request) {
        Ok(value) => value,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let Some(server) = args.get("server").and_then(Value::as_str) else {
        return ToolResult::invalid_input(
            request,
            "read_mcp_resource requires string field 'server'",
        );
    };
    let Some(uri) = args.get("uri").and_then(Value::as_str) else {
        return ToolResult::invalid_input(request, "read_mcp_resource requires string field 'uri'");
    };
    let should_cancel = || ctx.is_cancelled();

    let result = if ctx.should_cancel.is_some() {
        registry.read_resource_or_cancel(server, uri, &should_cancel)
    } else {
        registry
            .read_resource(server, uri)
            .map_err(McpRequestError::Failed)
    };
    match result {
        Ok(result) => match serde_json::to_string(&result) {
            Ok(output) => ToolResult::completed(request, output, false),
            Err(error) => ToolResult::failed(
                request,
                format!("failed to serialize MCP resource content: {error}"),
                None,
            ),
        },
        Err(McpRequestError::Cancelled) => {
            ToolResult::cancelled(request, MCP_TOOL_CALL_CANCELLED, None)
        }
        Err(McpRequestError::Failed(error)) => ToolResult::failed(request, error, None),
    }
}

fn parse_json_arguments(request: &ToolRequest) -> Result<Value, String> {
    request
        .raw_arguments
        .as_deref()
        .map(serde_json::from_str::<Value>)
        .transpose()
        .map_err(|error| format!("invalid tool arguments JSON: {error}"))
        .map(|value| value.unwrap_or_else(|| json!({})))
}

struct McpProxyTool {
    tool: McpTool,
    spec: ToolSpec,
}

impl McpProxyTool {
    fn new(tool: McpTool) -> Self {
        let spec = ToolSpec {
            name: ToolName::from_str(&tool.schema_name)
                .unwrap_or_else(|| ToolName::Mcp(tool.schema_name.clone())),
            aliases: Vec::new(),
            description: tool
                .description
                .clone()
                .unwrap_or_else(|| format!("MCP tool {} from {}", tool.name, tool.server)),
            input_schema: tool.input_schema.clone(),
            output_schema: None,
            capabilities: CapabilitySet::filesystem_write(),
            exposure: ToolExposure::Direct,
            result_semantics: ResultSemantics::Standard,
            interrupt_semantics: InterruptSemantics::CooperativeCancel,
            replay_semantics: ReplaySemantics::IndeterminateAfterStart,
            renderer: RendererHint::Write,
            concurrent_safe: false,
        };
        Self { tool, spec }
    }
}

impl Tool for McpProxyTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrent_safe(&self, _input: &ToolRequest) -> bool {
        false
    }

    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        let Some(registry) = ctx.mcp_registry else {
            return ToolResult::failed(request, "MCP registry is not initialized", None);
        };
        let Some(tool_ref) = registry.resolve_tool(&self.tool.schema_name) else {
            return ToolResult::failed(
                request,
                format!("unknown MCP tool: {}", self.tool.schema_name),
                None,
            );
        };
        let arguments = match request
            .raw_arguments
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
        {
            Ok(Some(value)) => value,
            Ok(None) => Value::Object(Default::default()),
            Err(error) => {
                return ToolResult::failed(
                    request,
                    format!("invalid MCP arguments JSON: {error}"),
                    None,
                );
            }
        };

        let cancellation_observed = Cell::new(false);
        let should_cancel = || {
            let cancelled = ctx.is_cancelled();
            cancellation_observed.set(cancellation_observed.get() || cancelled);
            cancelled
        };
        let call_result = if let Some(handler) = ctx.mcp_elicitation_handler {
            if ctx.should_cancel.is_some() {
                registry.call_tool_with_elicitation_handler_or_cancel(
                    &tool_ref,
                    arguments,
                    Some(handler),
                    &should_cancel,
                )
            } else {
                registry.call_tool_with_elicitation_handler(&tool_ref, arguments, Some(handler))
            }
        } else if ctx.should_cancel.is_some() {
            registry.call_tool_or_cancel(&tool_ref, arguments, &should_cancel)
        } else {
            registry.call_tool(&tool_ref, arguments)
        };

        mcp_call_result_to_tool_result(request, call_result, cancellation_observed.get())
    }
}

const MCP_TOOL_CALL_CANCELLED: &str = "MCP tool call cancelled";

fn mcp_call_result_to_tool_result(
    request: &ToolRequest,
    call_result: Result<McpCallOutput, String>,
    cancellation_observed: bool,
) -> ToolResult {
    match call_result {
        Ok(result) if result.is_error => ToolResult::failed(request, result.output, None),
        Ok(result) => ToolResult::completed(request, result.output, false),
        Err(error) if cancellation_observed && error == MCP_TOOL_CALL_CANCELLED => {
            ToolResult::cancelled(request, error, None)
        }
        Err(error) => ToolResult::failed(request, error, None),
    }
}

struct ExternalTool {
    tool: ExternalToolConfig,
    spec: ToolSpec,
}

impl ExternalTool {
    fn new(tool: ExternalToolConfig) -> Self {
        let spec = ToolSpec {
            name: ToolName::plain(&tool.name),
            aliases: Vec::new(),
            description: tool.description.clone(),
            input_schema: tool.parameters_schema(),
            output_schema: None,
            capabilities: capability_set_for_action_kind(tool.action_kind),
            exposure: ToolExposure::Direct,
            result_semantics: ResultSemantics::Standard,
            interrupt_semantics: InterruptSemantics::CooperativeCancel,
            replay_semantics: ReplaySemantics::IndeterminateAfterStart,
            renderer: renderer_for_action_kind(tool.action_kind),
            concurrent_safe: matches!(tool.action_kind, ActionKind::Read),
        };
        Self { tool, spec }
    }
}

impl Tool for ExternalTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrent_safe(&self, input: &ToolRequest) -> bool {
        self.is_read_only(input) && self.spec.concurrent_safe
    }

    fn execute(&self, request: &ToolRequest, ctx: &ToolContext<'_>) -> ToolResult {
        external::execute_external_tool_with_policy_or_cancel(
            &self.tool,
            request,
            ctx.cwd,
            ctx.output_truncation,
            ctx.shell_timeout,
            || ctx.is_cancelled(),
        )
    }
}

pub fn tool_name_from_schema_name(name: &str) -> Option<ToolName> {
    ToolName::from_str(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_cancellation_mapping_requires_exact_error_and_observation() {
        let request = request(ToolName::Mcp("mcp__docs__search".to_string()), "{}");

        let cancelled = mcp_call_result_to_tool_result(
            &request,
            Err(MCP_TOOL_CALL_CANCELLED.to_string()),
            true,
        );
        assert_eq!(
            cancelled.status,
            orca_core::tool_types::ToolStatus::Cancelled
        );
        assert_eq!(
            cancelled.kind,
            orca_core::tool_types::ToolResultKind::Cancelled
        );

        for (error, cancellation_observed) in [
            (MCP_TOOL_CALL_CANCELLED, false),
            ("remote error: MCP tool call cancelled", true),
            ("MCP tool call cancelled after response", true),
        ] {
            let result = mcp_call_result_to_tool_result(
                &request,
                Err(error.to_string()),
                cancellation_observed,
            );
            assert_eq!(
                result.status,
                orca_core::tool_types::ToolStatus::Failed,
                "error={error:?}, cancellation_observed={cancellation_observed}"
            );
        }
    }

    #[test]
    fn registry_control_semantics_are_conservative_by_resolved_identity() {
        let registry = default_tool_registry();

        let bash = registry.resolve("bash").expect("bash tool");
        assert_eq!(
            bash.spec.interrupt_semantics,
            InterruptSemantics::CooperativeCancel
        );
        assert_eq!(
            bash.spec.replay_semantics,
            ReplaySemantics::IndeterminateAfterStart
        );

        for name in [
            "read_file",
            "list_files",
            "glob",
            "grep",
            "git_status",
            "subagent_status",
            "task_list",
            "workflow_read_messages",
            "workflow_list_tasks",
            "get_goal",
            "list_skills",
            "read_skill",
        ] {
            let resolved = registry.resolve(name).expect("local read tool");
            assert_eq!(
                resolved.spec.interrupt_semantics,
                InterruptSemantics::WaitForTerminal,
                "{name} interrupt policy"
            );
            assert_eq!(
                resolved.spec.replay_semantics,
                ReplaySemantics::SafeToRetry,
                "{name} replay policy"
            );
        }

        let web_search = registry.resolve("web_search").expect("web search tool");
        assert_eq!(
            web_search.spec.interrupt_semantics,
            InterruptSemantics::CooperativeCancel
        );
        assert_eq!(
            web_search.spec.replay_semantics,
            ReplaySemantics::IndeterminateAfterStart
        );

        for name in [
            "write_file",
            "subagent",
            "request_permissions",
            "ask_user_question",
            "list_mcp_resources",
            "read_mcp_resource",
        ] {
            let resolved = registry.resolve(name).expect("conservative tool");
            assert_eq!(
                resolved.spec.replay_semantics,
                ReplaySemantics::IndeterminateAfterStart,
                "{name} replay policy"
            );
        }

        let external = ExternalTool::new(ExternalToolConfig {
            name: "inspect_remote".to_string(),
            description: "external read-like tool".to_string(),
            action_kind: ActionKind::Read,
            command: "printf inspected".to_string(),
            schema: json!({}),
        });
        assert_eq!(
            external.spec.interrupt_semantics,
            InterruptSemantics::CooperativeCancel
        );
        assert_eq!(
            external.spec.replay_semantics,
            ReplaySemantics::IndeterminateAfterStart
        );

        let mcp = McpProxyTool::new(McpTool {
            server: "docs".to_string(),
            name: "search".to_string(),
            schema_name: "mcp__docs__search".to_string(),
            description: Some("search docs".to_string()),
            input_schema: json!({"type": "object", "properties": {}}),
        });
        assert_eq!(
            mcp.spec.interrupt_semantics,
            InterruptSemantics::CooperativeCancel
        );
        assert_eq!(
            mcp.spec.replay_semantics,
            ReplaySemantics::IndeterminateAfterStart
        );

        assert!(registry.iter().all(|tool| {
            tool.spec().interrupt_semantics != InterruptSemantics::DetachAndObserve
        }));
        assert!(registry.iter().all(|tool| {
            tool.spec().replay_semantics != ReplaySemantics::SafeToRetry
                || tool.spec().capabilities.is_read_only()
        }));

        let mut safe_to_retry = registry
            .iter()
            .filter(|tool| tool.spec().replay_semantics == ReplaySemantics::SafeToRetry)
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        safe_to_retry.sort_unstable();
        assert_eq!(
            safe_to_retry,
            [
                "get_goal",
                "git_status",
                "glob",
                "grep",
                "list_skills",
                "read_file",
                "read_skill",
                "subagent_status",
                "task_list",
                "workflow_list_tasks",
                "workflow_read_messages",
            ]
        );
    }

    fn request(name: ToolName, raw_arguments: &str) -> ToolRequest {
        ToolRequest {
            id: "call-1".to_string(),
            name,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(raw_arguments.to_string()),
        }
    }

    #[test]
    fn registry_rejects_arguments_that_do_not_match_tool_schema() {
        let registry = default_tool_registry();
        let result = registry.execute(
            &request(
                ToolName::UpdatePlan,
                r#"{"plan":[{"completed":"Inspect references"}]}"#,
            ),
            &ToolContext::new(Path::new(".")),
        );

        assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("tool arguments failed schema validation"),
            "error={:?}",
            result.error
        );
    }

    #[test]
    fn update_goal_schema_accepts_structured_evidence_and_blocker() {
        validate_tool_request(
            default_tool_registry(),
            &request(
                ToolName::UpdateGoal,
                r#"{
                    "status":"blocked",
                    "reason":"waiting for a credential",
                    "evidence":[{
                        "kind":"observation",
                        "summary":"credential lookup returned no value",
                        "target":"DEEPSEEK_API_KEY"
                    }],
                    "blocker":{
                        "kind":"missing_authority",
                        "summary":"the user must provide a credential"
                    }
                }"#,
            ),
        )
        .expect("structured terminal evidence must match advertised schema");
    }

    #[test]
    fn registry_rejects_glob_arguments_that_match_no_any_of_branch() {
        let registry = default_tool_registry();
        let result =
            validate_tool_request(registry, &request(ToolName::Glob, r#"{"mode":"fuzzy"}"#));

        assert!(
            result
                .expect_err("anyOf should reject missing query")
                .contains("expected at least one anyOf schema to match"),
        );
    }

    #[test]
    fn update_plan_accepts_null_explanation() {
        // DeepSeek strict mode forces `explanation` to always be emitted,
        // sometimes as null; the base schema must accept that shape too.
        let registry = default_tool_registry();
        validate_tool_request(
            registry,
            &request(
                ToolName::UpdatePlan,
                r#"{"explanation":null,"plan":[{"step":"a","status":"pending"}]}"#,
            ),
        )
        .expect("null explanation must validate");
    }

    #[test]
    fn validation_errors_teach_allowed_and_required_properties() {
        let registry = default_tool_registry();

        let unexpected = validate_tool_request(
            registry,
            &request(
                ToolName::UpdatePlan,
                r#"{"plan":[{"status":"completed","step":"a","extra":true}]}"#,
            ),
        )
        .expect_err("unexpected property must be rejected");
        assert!(
            unexpected.contains(r#"unexpected property "extra" (allowed: "status", "step")"#),
            "error should list allowed properties: {unexpected}"
        );

        let missing = validate_tool_request(
            registry,
            &request(ToolName::UpdatePlan, r#"{"plan":[{"step":"a"}]}"#),
        )
        .expect_err("missing status must be rejected");
        assert!(
            missing.contains(r#"missing required property "status" (required: "status", "step")"#),
            "error should list required properties: {missing}"
        );
    }

    #[test]
    fn validator_rejects_arguments_that_match_multiple_one_of_branches() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "id": { "type": "integer" }
            },
            "oneOf": [
                { "required": ["path"] },
                { "required": ["id"] }
            ],
            "additionalProperties": false
        });
        let result = validate_arguments(
            &request(
                ToolName::plain("custom_oneof"),
                r#"{"path":"README.md","id":1}"#,
            ),
            &schema,
        );

        assert!(
            result
                .expect_err("oneOf should reject ambiguous branch match")
                .contains("expected exactly one oneOf schema to match"),
        );
    }

    #[test]
    fn validator_accepts_arguments_that_match_any_of_branch() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "id": { "type": "integer" }
            },
            "anyOf": [
                { "required": ["path"] },
                { "required": ["id"] }
            ],
            "additionalProperties": false
        });

        let result = validate_arguments(
            &request(ToolName::plain("custom_anyof"), r#"{"path":"README.md"}"#),
            &schema,
        );

        assert!(result.is_ok(), "result={result:?}");
    }

    #[test]
    fn validator_rejects_arguments_that_match_no_any_of_branch() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "id": { "type": "integer" }
            },
            "anyOf": [
                { "required": ["path"] },
                { "required": ["id"] }
            ],
            "additionalProperties": false
        });

        let result =
            validate_arguments(&request(ToolName::plain("custom_anyof"), r#"{}"#), &schema);

        assert!(
            result
                .expect_err("anyOf should reject missing branch")
                .contains("expected at least one anyOf schema to match")
        );
    }

    #[test]
    fn workflow_mailbox_tools_have_autoedit_safe_action_kinds() {
        let registry = default_tool_registry();

        assert_eq!(
            registry
                .get("Workflow")
                .expect("workflow tool")
                .action_kind(),
            ActionKind::Agent
        );
        assert_eq!(
            registry
                .get("workflow_send_message")
                .expect("workflow_send_message tool")
                .action_kind(),
            ActionKind::Write
        );
        assert_eq!(
            registry
                .get("workflow_read_messages")
                .expect("workflow_read_messages tool")
                .action_kind(),
            ActionKind::Read
        );
        assert_eq!(
            registry
                .get("workflow_clear_messages")
                .expect("workflow_clear_messages tool")
                .action_kind(),
            ActionKind::Write
        );
    }

    #[test]
    fn workflow_task_list_tools_have_autoedit_safe_action_kinds() {
        let registry = default_tool_registry();

        assert_eq!(
            registry
                .get("workflow_create_task_list")
                .expect("workflow_create_task_list tool")
                .action_kind(),
            ActionKind::Write
        );
        assert_eq!(
            registry
                .get("workflow_claim_task")
                .expect("workflow_claim_task tool")
                .action_kind(),
            ActionKind::Write
        );
        assert_eq!(
            registry
                .get("workflow_complete_task")
                .expect("workflow_complete_task tool")
                .action_kind(),
            ActionKind::Write
        );
        assert_eq!(
            registry
                .get("workflow_list_tasks")
                .expect("workflow_list_tasks tool")
                .action_kind(),
            ActionKind::Read
        );
    }

    #[test]
    fn package3_task_tools_are_model_visible_with_safe_action_kinds() {
        let registry = default_tool_registry();

        let task_list = registry.get("task_list").expect("task_list tool");
        assert_eq!(task_list.action_kind(), ActionKind::Read);
        assert!(task_list.spec().exposure.is_model_visible());
        assert!(task_list.is_concurrent_safe(&request(ToolName::TaskList, "{}")));

        let task_stop = registry.get("task_stop").expect("task_stop tool");
        assert_eq!(task_stop.action_kind(), ActionKind::Write);
        assert!(task_stop.spec().exposure.is_model_visible());
        assert!(
            !task_stop.is_concurrent_safe(&request(ToolName::TaskStop, r#"{"task_id":"task-1"}"#))
        );
        assert!(
            task_stop
                .spec()
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .expect("properties")
                .contains_key("shell_id"),
            "task_stop should accept package 3's deprecated shell_id alias"
        );
    }

    #[test]
    fn unified_exec_tools_are_model_visible_and_serialized() {
        let registry = default_tool_registry();
        let exec = registry.resolve("exec_command").expect("exec_command");
        let write = registry.resolve("write_stdin").expect("write_stdin");

        assert!(exec.tool.spec().exposure.is_model_visible());
        assert!(write.tool.spec().exposure.is_model_visible());
        assert_eq!(exec.tool.action_kind(), ActionKind::Shell);
        assert_eq!(write.tool.action_kind(), ActionKind::Read);
        assert!(
            !exec
                .tool
                .is_concurrent_safe(&request(ToolName::ExecCommand, r#"{"cmd":"sleep 1"}"#,))
        );
        assert!(!write.tool.is_concurrent_safe(&request(
            ToolName::WriteStdin,
            r#"{"session_id":"shell-1"}"#,
        )));
    }

    #[test]
    fn workflow_and_subagent_schemas_document_runtime_contracts() {
        let registry = default_tool_registry();

        let workflow = registry.get("Workflow").expect("workflow tool");
        let workflow_script_description =
            workflow.spec().input_schema["properties"]["script"]["description"]
                .as_str()
                .expect("workflow script description");
        assert!(workflow_script_description.contains("Do not export `run`"));
        assert!(workflow_script_description.contains("tasks: [{ prompt:"));
        assert!(workflow.spec().input_schema["properties"]["tokenBudget"].is_object());

        let subagent = registry.get("subagent").expect("subagent tool");
        let subagent_mode_description =
            subagent.spec().input_schema["properties"]["mode"]["description"]
                .as_str()
                .expect("subagent mode description");
        assert!(subagent_mode_description.contains("long-running"));
        assert!(subagent_mode_description.contains("task_list"));
    }

    #[test]
    fn request_permissions_tool_is_model_visible_runtime_special() {
        let registry = default_tool_registry();

        let tool = registry
            .get("request_permissions")
            .expect("request_permissions tool");
        assert_eq!(tool.action_kind(), ActionKind::Write);
        assert!(tool.spec().exposure.is_model_visible());
        assert!(!tool.is_concurrent_safe(&request(
            ToolName::RequestPermissions,
            r#"{"reason":"write generated files","permissions":{"fileSystem":{"write":["/tmp/orca-extra"]}}}"#
        )));
        assert!(
            tool.spec()
                .input_schema
                .pointer("/properties/permissions/properties/fileSystem/properties/write")
                .is_some(),
            "schema should expose Codex-style fileSystem.write permission requests"
        );
        assert_eq!(
            tool.spec()
                .input_schema
                .pointer("/properties/permissions/properties/fileSystem/properties/entries/items/properties/access/enum")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default(),
            vec![json!("read"), json!("write"), json!("readWrite")],
            "schema should expose Codex-style fileSystem.entries access modes"
        );
        assert_eq!(
            tool.spec()
                .input_schema
                .pointer("/properties/permissions/properties/network/properties/domains/additionalProperties/enum")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default(),
            vec![json!("allow"), json!("deny")],
            "schema should expose permission-profile network domain requests"
        );
    }

    #[test]
    fn mcp_resource_tools_are_model_visible_readonly_tools() {
        let registry = default_tool_registry();

        let list = registry
            .get("list_mcp_resources")
            .expect("list_mcp_resources tool");
        assert_eq!(list.action_kind(), ActionKind::Read);
        assert!(list.spec().exposure.is_model_visible());
        assert!(
            list.is_concurrent_safe(&request(ToolName::ListMcpResources, r#"{"server":"docs"}"#))
        );
        assert!(
            list.spec()
                .input_schema
                .pointer("/properties/server")
                .is_some(),
            "list_mcp_resources should accept an optional server filter"
        );

        let templates = registry
            .get("list_mcp_resource_templates")
            .expect("list_mcp_resource_templates tool");
        assert_eq!(templates.action_kind(), ActionKind::Read);
        assert!(templates.spec().exposure.is_model_visible());
        assert!(templates.is_concurrent_safe(&request(
            ToolName::ListMcpResourceTemplates,
            r#"{"server":"docs"}"#
        )));
        assert!(
            templates
                .spec()
                .input_schema
                .pointer("/properties/server")
                .is_some(),
            "list_mcp_resource_templates should accept an optional server filter"
        );

        let read = registry
            .get("read_mcp_resource")
            .expect("read_mcp_resource tool");
        assert_eq!(read.action_kind(), ActionKind::Read);
        assert!(read.spec().exposure.is_model_visible());
        assert!(read.is_concurrent_safe(&request(
            ToolName::ReadMcpResource,
            r#"{"server":"docs","uri":"memo://one"}"#
        )));
        let required = read
            .spec()
            .input_schema
            .pointer("/required")
            .and_then(Value::as_array)
            .expect("required fields");
        assert!(required.contains(&Value::String("server".to_string())));
        assert!(required.contains(&Value::String("uri".to_string())));
    }

    #[test]
    fn request_permissions_schema_accepts_null_overlay_sections() {
        let registry = default_tool_registry();
        let result = registry.execute(
            &request(
                ToolName::RequestPermissions,
                r#"{"reason":"temporary write","permissions":{"fileSystem":{"read":null,"write":["/tmp/orca-extra"]},"network":null}}"#,
            ),
            &ToolContext::new(Path::new(".")),
        );

        assert!(
            !result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("tool arguments failed schema validation"),
            "schema should accept Codex-style null overlay sections: {:?}",
            result.error
        );
    }
}
