use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use crate::approval_types::ActionKind;

pub const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolName {
    ReadFile,
    ListFiles,
    Glob,
    Grep,
    Bash,
    ExecCommand,
    WriteStdin,
    Edit,
    WriteFile,
    GitStatus,
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
    WebSearch,
    GetGoal,
    CreateGoal,
    UpdateGoal,
    UpdatePlan,
    AskUserQuestion,
    RequestUserInput,
    RequestPermissions,
    ListMcpResources,
    ListMcpResourceTemplates,
    ReadMcpResource,
    ListSkills,
    ReadSkill,
    Namespaced {
        namespace: String,
        name: String,
        serialized: String,
    },
    Mcp(String),
    External(String),
}

impl ToolName {
    pub fn plain(name: impl Into<String>) -> Self {
        let name = name.into();
        match name.as_str() {
            "read_file" => Self::ReadFile,
            "list_files" => Self::ListFiles,
            "glob" => Self::Glob,
            "grep" => Self::Grep,
            "bash" => Self::Bash,
            "exec_command" => Self::ExecCommand,
            "write_stdin" => Self::WriteStdin,
            "edit" => Self::Edit,
            "write_file" => Self::WriteFile,
            "git_status" => Self::GitStatus,
            "subagent" => Self::Subagent,
            "subagent_status" => Self::SubagentStatus,
            "task_list" => Self::TaskList,
            "task_stop" => Self::TaskStop,
            "WorkflowDraft" | "workflow_draft" => Self::WorkflowDraft,
            "WorkflowDraftAction" | "workflow_draft_action" => Self::WorkflowDraftAction,
            "Workflow" | "workflow" => Self::Workflow,
            "workflow_send_message" => Self::WorkflowSendMessage,
            "workflow_read_messages" => Self::WorkflowReadMessages,
            "workflow_clear_messages" => Self::WorkflowClearMessages,
            "workflow_create_task_list" => Self::WorkflowCreateTaskList,
            "workflow_claim_task" => Self::WorkflowClaimTask,
            "workflow_complete_task" => Self::WorkflowCompleteTask,
            "workflow_list_tasks" => Self::WorkflowListTasks,
            "web_search" => Self::WebSearch,
            "get_goal" => Self::GetGoal,
            "create_goal" => Self::CreateGoal,
            "update_goal" => Self::UpdateGoal,
            "update_plan" => Self::UpdatePlan,
            "ask_user_question" => Self::AskUserQuestion,
            "request_user_input" => Self::RequestUserInput,
            "request_permissions" => Self::RequestPermissions,
            "list_mcp_resources" => Self::ListMcpResources,
            "list_mcp_resource_templates" => Self::ListMcpResourceTemplates,
            "read_mcp_resource" => Self::ReadMcpResource,
            "list_skills" => Self::ListSkills,
            "read_skill" => Self::ReadSkill,
            other => Self::External(other.to_string()),
        }
    }

    pub fn namespaced(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        let namespace = namespace.into();
        let name = name.into();
        let serialized = format!("{namespace}__{name}");
        Self::Namespaced {
            namespace,
            name,
            serialized,
        }
    }

    pub fn namespace(&self) -> Option<&str> {
        match self {
            Self::Namespaced { namespace, .. } => Some(namespace),
            Self::Mcp(name) => name.rsplit_once("__").map(|(namespace, _)| namespace),
            _ => None,
        }
    }

    pub fn local_name(&self) -> &str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListFiles => "list_files",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Bash => "bash",
            Self::ExecCommand => "exec_command",
            Self::WriteStdin => "write_stdin",
            Self::Edit => "edit",
            Self::WriteFile => "write_file",
            Self::GitStatus => "git_status",
            Self::Subagent => "subagent",
            Self::SubagentStatus => "subagent_status",
            Self::TaskList => "task_list",
            Self::TaskStop => "task_stop",
            Self::WorkflowDraft => "WorkflowDraft",
            Self::WorkflowDraftAction => "WorkflowDraftAction",
            Self::Workflow => "Workflow",
            Self::WorkflowSendMessage => "workflow_send_message",
            Self::WorkflowReadMessages => "workflow_read_messages",
            Self::WorkflowClearMessages => "workflow_clear_messages",
            Self::WorkflowCreateTaskList => "workflow_create_task_list",
            Self::WorkflowClaimTask => "workflow_claim_task",
            Self::WorkflowCompleteTask => "workflow_complete_task",
            Self::WorkflowListTasks => "workflow_list_tasks",
            Self::WebSearch => "web_search",
            Self::GetGoal => "get_goal",
            Self::CreateGoal => "create_goal",
            Self::UpdateGoal => "update_goal",
            Self::UpdatePlan => "update_plan",
            Self::AskUserQuestion => "ask_user_question",
            Self::RequestUserInput => "request_user_input",
            Self::RequestPermissions => "request_permissions",
            Self::ListMcpResources => "list_mcp_resources",
            Self::ListMcpResourceTemplates => "list_mcp_resource_templates",
            Self::ReadMcpResource => "read_mcp_resource",
            Self::ListSkills => "list_skills",
            Self::ReadSkill => "read_skill",
            Self::Namespaced { name, .. } => name,
            Self::Mcp(name) => name
                .rsplit_once("__")
                .map(|(_, local)| local)
                .unwrap_or(name),
            Self::External(name) => name,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListFiles => "list_files",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Bash => "bash",
            Self::ExecCommand => "exec_command",
            Self::WriteStdin => "write_stdin",
            Self::Edit => "edit",
            Self::WriteFile => "write_file",
            Self::GitStatus => "git_status",
            Self::Subagent => "subagent",
            Self::SubagentStatus => "subagent_status",
            Self::TaskList => "task_list",
            Self::TaskStop => "task_stop",
            Self::WorkflowDraft => "WorkflowDraft",
            Self::WorkflowDraftAction => "WorkflowDraftAction",
            Self::Workflow => "Workflow",
            Self::WorkflowSendMessage => "workflow_send_message",
            Self::WorkflowReadMessages => "workflow_read_messages",
            Self::WorkflowClearMessages => "workflow_clear_messages",
            Self::WorkflowCreateTaskList => "workflow_create_task_list",
            Self::WorkflowClaimTask => "workflow_claim_task",
            Self::WorkflowCompleteTask => "workflow_complete_task",
            Self::WorkflowListTasks => "workflow_list_tasks",
            Self::WebSearch => "web_search",
            Self::GetGoal => "get_goal",
            Self::CreateGoal => "create_goal",
            Self::UpdateGoal => "update_goal",
            Self::UpdatePlan => "update_plan",
            Self::AskUserQuestion => "ask_user_question",
            Self::RequestUserInput => "request_user_input",
            Self::RequestPermissions => "request_permissions",
            Self::ListMcpResources => "list_mcp_resources",
            Self::ListMcpResourceTemplates => "list_mcp_resource_templates",
            Self::ReadMcpResource => "read_mcp_resource",
            Self::ListSkills => "list_skills",
            Self::ReadSkill => "read_skill",
            Self::Namespaced { serialized, .. } => serialized,
            Self::Mcp(name) | Self::External(name) => name,
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        if value.starts_with("mcp__") {
            return Some(Self::Mcp(value.to_string()));
        }
        if let Some((namespace, name)) = parse_namespaced_tool(value) {
            return Some(Self::namespaced(namespace, name));
        }
        Some(match value {
            "read_file" => Self::ReadFile,
            "list_files" => Self::ListFiles,
            "glob" => Self::Glob,
            "grep" => Self::Grep,
            "bash" => Self::Bash,
            "exec_command" => Self::ExecCommand,
            "write_stdin" => Self::WriteStdin,
            "edit" => Self::Edit,
            "write_file" => Self::WriteFile,
            "git_status" => Self::GitStatus,
            "subagent" => Self::Subagent,
            "subagent_status" => Self::SubagentStatus,
            "task_list" => Self::TaskList,
            "task_stop" => Self::TaskStop,
            "WorkflowDraft" | "workflow_draft" => Self::WorkflowDraft,
            "WorkflowDraftAction" | "workflow_draft_action" => Self::WorkflowDraftAction,
            "Workflow" | "workflow" => Self::Workflow,
            "workflow_send_message" => Self::WorkflowSendMessage,
            "workflow_read_messages" => Self::WorkflowReadMessages,
            "workflow_clear_messages" => Self::WorkflowClearMessages,
            "workflow_create_task_list" => Self::WorkflowCreateTaskList,
            "workflow_claim_task" => Self::WorkflowClaimTask,
            "workflow_complete_task" => Self::WorkflowCompleteTask,
            "workflow_list_tasks" => Self::WorkflowListTasks,
            "web_search" => Self::WebSearch,
            "get_goal" => Self::GetGoal,
            "create_goal" => Self::CreateGoal,
            "update_goal" => Self::UpdateGoal,
            "update_plan" => Self::UpdatePlan,
            "ask_user_question" => Self::AskUserQuestion,
            "request_user_input" => Self::RequestUserInput,
            "request_permissions" => Self::RequestPermissions,
            "list_mcp_resources" => Self::ListMcpResources,
            "list_mcp_resource_templates" => Self::ListMcpResourceTemplates,
            "read_mcp_resource" => Self::ReadMcpResource,
            "list_skills" => Self::ListSkills,
            "read_skill" => Self::ReadSkill,
            other => Self::External(other.to_string()),
        })
    }

    pub fn is_builtin(&self, builtin: &str) -> bool {
        self.namespace().is_none() && self.as_str() == builtin
    }

    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::ReadFile
                | Self::ListFiles
                | Self::Glob
                | Self::Grep
                | Self::GitStatus
                | Self::SubagentStatus
                | Self::TaskList
                | Self::WorkflowReadMessages
                | Self::WorkflowListTasks
                | Self::GetGoal
                | Self::AskUserQuestion
                | Self::RequestUserInput
                | Self::ListMcpResources
                | Self::ListMcpResourceTemplates
                | Self::ReadMcpResource
                | Self::ListSkills
                | Self::ReadSkill
        )
    }
}

fn parse_namespaced_tool(value: &str) -> Option<(&str, &str)> {
    let (namespace, name) = value.rsplit_once("__")?;
    if !namespace.is_empty() && !name.is_empty() {
        Some((namespace, name))
    } else {
        None
    }
}

impl Serialize for ToolName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown tool name: {value}")))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolCapability {
    FsRead,
    FsList,
    FsSearch,
    FsWrite,
    ShellExecute,
    GitInspect,
    NetworkSearch,
    AgentDelegate,
    WorkflowRun,
    TaskRead,
    TaskControl,
    TerminalTransport,
    PlanUpdate,
    GoalUpdate,
    UserInputRequest,
    PermissionRequest,
    McpResourceRead,
    SkillRead,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilitySet {
    capabilities: Vec<ToolCapability>,
}

impl CapabilitySet {
    pub fn new(capabilities: Vec<ToolCapability>) -> Self {
        Self { capabilities }
    }

    pub fn read_only_fs() -> Self {
        Self::new(vec![ToolCapability::FsRead])
    }

    pub fn filesystem_write() -> Self {
        Self::new(vec![ToolCapability::FsWrite])
    }

    pub fn shell_execute() -> Self {
        Self::new(vec![ToolCapability::ShellExecute])
    }

    pub fn network_search() -> Self {
        Self::new(vec![ToolCapability::NetworkSearch])
    }

    pub fn agent_delegate() -> Self {
        Self::new(vec![ToolCapability::AgentDelegate])
    }

    pub fn contains(&self, capability: ToolCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn is_read_only(&self) -> bool {
        !self.capabilities.is_empty()
            && self.capabilities.iter().all(|capability| {
                matches!(
                    capability,
                    ToolCapability::FsRead
                        | ToolCapability::FsList
                        | ToolCapability::FsSearch
                        | ToolCapability::GitInspect
                        | ToolCapability::TaskRead
                        | ToolCapability::PlanUpdate
                        | ToolCapability::GoalUpdate
                        | ToolCapability::UserInputRequest
                        | ToolCapability::McpResourceRead
                        | ToolCapability::SkillRead
                )
            })
    }

    pub fn action_kind(&self) -> ActionKind {
        if self.contains(ToolCapability::ShellExecute) {
            ActionKind::Shell
        } else if self.contains(ToolCapability::FsWrite) {
            ActionKind::Write
        } else if self.contains(ToolCapability::TaskControl) {
            ActionKind::Write
        } else if self.contains(ToolCapability::PermissionRequest) {
            ActionKind::Write
        } else if self.contains(ToolCapability::NetworkSearch) {
            ActionKind::Network
        } else if self.contains(ToolCapability::AgentDelegate)
            || self.contains(ToolCapability::WorkflowRun)
        {
            ActionKind::Agent
        } else {
            ActionKind::Read
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolExposure {
    Direct,
    Deferred,
    ModelOnly,
    Hidden,
}

impl ToolExposure {
    pub fn is_model_visible(self) -> bool {
        matches!(self, Self::Direct | Self::ModelOnly)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RendererHint {
    FileRead,
    FileList,
    FileSearch,
    Shell,
    Write,
    Network,
    Agent,
    State,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultSemantics {
    Standard,
    EmptyIsSuccess,
    NoMatchesIsSuccess,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptSemantics {
    CooperativeCancel,
    WaitForTerminal,
    DetachAndObserve,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaySemantics {
    SafeToRetry,
    IdempotentWithKey,
    IndeterminateAfterStart,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolControlSemantics {
    pub interrupt: InterruptSemantics,
    pub replay: ReplaySemantics,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultKind {
    Success,
    Empty,
    NoMatches,
    Truncated,
    PermissionDenied,
    InvalidInput,
    RuntimeError,
    Cancelled,
    Indeterminate,
}

impl ToolResultKind {
    pub fn success() -> Self {
        Self::Success
    }

    pub fn is_success(&self) -> bool {
        *self == Self::Success
    }

    pub fn status(self) -> ToolStatus {
        match self {
            Self::Success | Self::Empty | Self::NoMatches | Self::Truncated => {
                ToolStatus::Completed
            }
            Self::PermissionDenied => ToolStatus::Denied,
            Self::InvalidInput | Self::RuntimeError => ToolStatus::Failed,
            Self::Cancelled => ToolStatus::Cancelled,
            Self::Indeterminate => ToolStatus::Indeterminate,
        }
    }
}

impl Default for ToolResultKind {
    fn default() -> Self {
        Self::Success
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: ToolName,
    pub aliases: Vec<ToolName>,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: Option<serde_json::Value>,
    pub capabilities: CapabilitySet,
    pub exposure: ToolExposure,
    pub result_semantics: ResultSemantics,
    pub interrupt_semantics: InterruptSemantics,
    pub replay_semantics: ReplaySemantics,
    pub renderer: RendererHint,
    pub concurrent_safe: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolRequest {
    pub id: String,
    pub name: ToolName,
    pub action: ActionKind,
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_arguments: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Completed,
    Failed,
    Denied,
    NotImplemented,
    Cancelled,
    Indeterminate,
}

impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Denied => "denied",
            Self::NotImplemented => "not_implemented",
            Self::Cancelled => "cancelled",
            Self::Indeterminate => "indeterminate",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileChangePreview {
    UnifiedDiff {
        text: String,
        truncated: bool,
    },
    Omitted {
        path: String,
        max_input_bytes: usize,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTerminalSource {
    Observed,
    CompatibilityRepair,
}

impl ToolTerminalSource {
    fn is_observed(&self) -> bool {
        *self == Self::Observed
    }
}

impl Default for ToolTerminalSource {
    fn default() -> Self {
        Self::Observed
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationStarted {
    Yes,
    No,
    Unknown,
}

impl ToolInvocationStarted {
    fn is_unknown(&self) -> bool {
        *self == Self::Unknown
    }
}

impl Default for ToolInvocationStarted {
    fn default() -> Self {
        Self::Unknown
    }
}

#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolTerminal {
    pub status: ToolStatus,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    #[serde(
        default = "ToolResultKind::success",
        skip_serializing_if = "ToolResultKind::is_success"
    )]
    pub kind: ToolResultKind,
    #[serde(
        default,
        rename = "terminal_source",
        skip_serializing_if = "ToolTerminalSource::is_observed"
    )]
    pub source: ToolTerminalSource,
    #[serde(
        default,
        rename = "invocation_started",
        skip_serializing_if = "ToolInvocationStarted::is_unknown"
    )]
    pub started: ToolInvocationStarted,
}

impl ToolTerminal {
    pub fn try_from_parts(
        status: ToolStatus,
        error: Option<String>,
        exit_code: Option<i32>,
        truncated: bool,
        kind: Option<ToolResultKind>,
        source: ToolTerminalSource,
        started: ToolInvocationStarted,
    ) -> Result<Self, String> {
        let kind = kind.unwrap_or_else(|| default_kind_for_status(status));
        if !terminal_status_matches_kind(status, kind) {
            return Err(format!(
                "tool terminal status '{}' conflicts with result kind '{kind:?}'",
                status.as_str()
            ));
        }
        Ok(Self {
            status,
            error,
            exit_code,
            truncated,
            kind,
            source,
            started,
        })
    }

    fn new(
        status: ToolStatus,
        error: Option<String>,
        exit_code: Option<i32>,
        truncated: bool,
        kind: ToolResultKind,
        source: ToolTerminalSource,
        started: ToolInvocationStarted,
    ) -> Self {
        Self::try_from_parts(
            status,
            error,
            exit_code,
            truncated,
            Some(kind),
            source,
            started,
        )
        .expect("tool terminal constructors use matching status and result kind")
    }
}

#[derive(Deserialize)]
struct ToolTerminalWire {
    status: ToolStatus,
    error: Option<String>,
    exit_code: Option<i32>,
    truncated: bool,
    #[serde(default)]
    kind: Option<ToolResultKind>,
    #[serde(default, rename = "terminal_source")]
    source: ToolTerminalSource,
    #[serde(default, rename = "invocation_started")]
    started: ToolInvocationStarted,
}

impl<'de> Deserialize<'de> for ToolTerminal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ToolTerminalWire::deserialize(deserializer)?;
        Self::try_from_parts(
            wire.status,
            wire.error,
            wire.exit_code,
            wire.truncated,
            wire.kind,
            wire.source,
            wire.started,
        )
        .map_err(serde::de::Error::custom)
    }
}

fn default_kind_for_status(status: ToolStatus) -> ToolResultKind {
    match status {
        ToolStatus::Completed => ToolResultKind::Success,
        ToolStatus::Failed | ToolStatus::NotImplemented => ToolResultKind::RuntimeError,
        ToolStatus::Denied => ToolResultKind::PermissionDenied,
        ToolStatus::Cancelled => ToolResultKind::Cancelled,
        ToolStatus::Indeterminate => ToolResultKind::Indeterminate,
    }
}

fn terminal_status_matches_kind(status: ToolStatus, kind: ToolResultKind) -> bool {
    status == kind.status()
        || matches!(
            (status, kind),
            (ToolStatus::NotImplemented, ToolResultKind::RuntimeError)
        )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    pub id: String,
    pub name: ToolName,
    pub output: Option<String>,
    #[serde(flatten)]
    terminal: ToolTerminal,
    #[serde(skip)]
    pub file_change_preview: Option<Arc<FileChangePreview>>,
}

impl Deref for ToolResult {
    type Target = ToolTerminal;

    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl ToolResult {
    pub fn terminal(&self) -> &ToolTerminal {
        &self.terminal
    }

    pub fn append_error(&mut self, suffix: &str) {
        match self.terminal.error.as_mut() {
            Some(error) if !error.trim_end().is_empty() => error.push_str(suffix),
            _ => self.terminal.error = Some(suffix.trim_start().to_string()),
        }
    }

    pub fn set_truncated(&mut self, truncated: bool) {
        self.terminal.truncated = truncated;
    }

    pub fn completed(request: &ToolRequest, output: String, truncated: bool) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            output: Some(output),
            terminal: ToolTerminal::new(
                ToolStatus::Completed,
                None,
                Some(0),
                truncated,
                ToolResultKind::Success,
                ToolTerminalSource::Observed,
                ToolInvocationStarted::Yes,
            ),
            file_change_preview: None,
        }
    }

    pub fn completed_kind(
        request: &ToolRequest,
        output: String,
        truncated: bool,
        kind: ToolResultKind,
    ) -> Self {
        assert!(
            matches!(
                kind,
                ToolResultKind::Success
                    | ToolResultKind::Empty
                    | ToolResultKind::NoMatches
                    | ToolResultKind::Truncated
            ),
            "completed_kind requires a completed result kind"
        );
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            output: Some(output),
            terminal: ToolTerminal::new(
                kind.status(),
                None,
                Some(0),
                truncated,
                kind,
                ToolTerminalSource::Observed,
                ToolInvocationStarted::Yes,
            ),
            file_change_preview: None,
        }
    }

    pub fn failed(request: &ToolRequest, error: impl Into<String>, exit_code: Option<i32>) -> Self {
        Self::failed_with_started(request, error, exit_code, ToolInvocationStarted::Unknown)
    }

    pub fn failed_after_start(
        request: &ToolRequest,
        error: impl Into<String>,
        exit_code: Option<i32>,
    ) -> Self {
        Self::failed_with_started(request, error, exit_code, ToolInvocationStarted::Yes)
    }

    pub fn failed_before_start(
        request: &ToolRequest,
        error: impl Into<String>,
        exit_code: Option<i32>,
    ) -> Self {
        Self::failed_with_started(request, error, exit_code, ToolInvocationStarted::No)
    }

    fn failed_with_started(
        request: &ToolRequest,
        error: impl Into<String>,
        exit_code: Option<i32>,
        started: ToolInvocationStarted,
    ) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            output: None,
            terminal: ToolTerminal::new(
                ToolStatus::Failed,
                Some(error.into()),
                exit_code,
                false,
                ToolResultKind::RuntimeError,
                ToolTerminalSource::Observed,
                started,
            ),
            file_change_preview: None,
        }
    }

    pub fn invalid_input(request: &ToolRequest, error: impl Into<String>) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            output: None,
            terminal: ToolTerminal::new(
                ToolStatus::Failed,
                Some(error.into()),
                None,
                false,
                ToolResultKind::InvalidInput,
                ToolTerminalSource::Observed,
                ToolInvocationStarted::No,
            ),
            file_change_preview: None,
        }
    }

    pub fn denied(request: &ToolRequest, reason: impl Into<String>) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            output: None,
            terminal: ToolTerminal::new(
                ToolStatus::Denied,
                Some(reason.into()),
                None,
                false,
                ToolResultKind::PermissionDenied,
                ToolTerminalSource::Observed,
                ToolInvocationStarted::No,
            ),
            file_change_preview: None,
        }
    }

    pub fn cancelled(
        request: &ToolRequest,
        reason: impl Into<String>,
        exit_code: Option<i32>,
    ) -> Self {
        Self::cancelled_with_started(request, reason, exit_code, ToolInvocationStarted::Yes)
    }

    fn cancelled_with_started(
        request: &ToolRequest,
        reason: impl Into<String>,
        exit_code: Option<i32>,
        started: ToolInvocationStarted,
    ) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            output: None,
            terminal: ToolTerminal::new(
                ToolStatus::Cancelled,
                Some(reason.into()),
                exit_code,
                false,
                ToolResultKind::Cancelled,
                ToolTerminalSource::Observed,
                started,
            ),
            file_change_preview: None,
        }
    }

    pub fn cancelled_before_start(request: &ToolRequest, reason: impl AsRef<str>) -> Self {
        Self::cancelled_with_started(
            request,
            format!(
                "Tool invocation was not started because {}",
                reason.as_ref()
            ),
            None,
            ToolInvocationStarted::No,
        )
    }

    pub fn indeterminate(request: &ToolRequest, reason: impl Into<String>) -> Self {
        Self::indeterminate_with_started(request, reason, ToolInvocationStarted::Unknown)
    }

    pub fn indeterminate_after_start(request: &ToolRequest, reason: impl Into<String>) -> Self {
        Self::indeterminate_with_started(request, reason, ToolInvocationStarted::Yes)
    }

    fn indeterminate_with_started(
        request: &ToolRequest,
        reason: impl Into<String>,
        started: ToolInvocationStarted,
    ) -> Self {
        Self {
            id: request.id.clone(),
            name: request.name.clone(),
            output: None,
            terminal: ToolTerminal::new(
                ToolStatus::Indeterminate,
                Some(reason.into()),
                None,
                false,
                ToolResultKind::Indeterminate,
                ToolTerminalSource::Observed,
                started,
            ),
            file_change_preview: None,
        }
    }

    pub fn with_file_change_preview(mut self, preview: FileChangePreview) -> Self {
        self.file_change_preview = Some(Arc::new(preview));
        self
    }

    pub fn with_terminal_source(mut self, source: ToolTerminalSource) -> Self {
        self.terminal.source = source;
        self
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolOutputTruncation {
    Bytes { limit: usize },
    Tokens { limit: usize },
}

impl ToolOutputTruncation {
    pub fn bytes(limit: usize) -> Self {
        Self::Bytes { limit }
    }

    pub fn tokens(limit: usize) -> Self {
        Self::Tokens { limit }
    }

    pub fn limit(self) -> usize {
        match self {
            Self::Bytes { limit } | Self::Tokens { limit } => limit,
        }
    }

    pub fn normalized(self) -> Self {
        match self {
            Self::Bytes { limit } => Self::Bytes {
                limit: limit.max(1),
            },
            Self::Tokens { limit } => Self::Tokens {
                limit: limit.max(1),
            },
        }
    }
}

impl Default for ToolOutputTruncation {
    fn default() -> Self {
        Self::Bytes {
            limit: MAX_TOOL_OUTPUT_BYTES,
        }
    }
}

impl fmt::Display for ToolOutputTruncation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes { limit } => write!(f, "bytes:{limit}"),
            Self::Tokens { limit } => write!(f, "tokens:{limit}"),
        }
    }
}

pub fn truncate_output(output: String, max_bytes: usize) -> (String, bool) {
    truncate_output_with_policy(output, ToolOutputTruncation::bytes(max_bytes))
}

pub fn truncate_output_with_policy(output: String, policy: ToolOutputTruncation) -> (String, bool) {
    match policy.normalized() {
        ToolOutputTruncation::Bytes { limit } => truncate_output_bytes(output, limit),
        ToolOutputTruncation::Tokens { limit } => truncate_output_tokens(output, limit),
    }
}

fn truncate_output_bytes(output: String, max_bytes: usize) -> (String, bool) {
    if output.len() <= max_bytes {
        return (output, false);
    }

    let marker = "\n[... tool output micro-compacted ...]\n";
    if max_bytes <= marker.len() + 2 {
        let mut end = max_bytes;
        while end > 0 && !output.is_char_boundary(end) {
            end -= 1;
        }
        return (output[..end].to_string(), true);
    }

    let side_budget = (max_bytes - marker.len()) / 2;
    let mut head_end = side_budget;
    while !output.is_char_boundary(head_end) {
        head_end -= 1;
    }

    let mut tail_start = output.len().saturating_sub(side_budget);
    while !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    (
        format!("{}{}{}", &output[..head_end], marker, &output[tail_start..]),
        true,
    )
}

fn truncate_output_tokens(output: String, max_tokens: usize) -> (String, bool) {
    let original_tokens = approx_tool_tokens(&output);
    if original_tokens <= max_tokens {
        return (output, false);
    }

    let line_count = output.lines().count();
    let byte_budget = approx_bytes_for_tokens(max_tokens);
    let (snippet, _) = truncate_output_bytes(output, byte_budget.max(1));
    (
        format!(
            "Warning: truncated tool output\nOriginal token count: {original_tokens}\nOriginal line count: {line_count}\n\n{snippet}"
        ),
        true,
    )
}

fn approx_tool_tokens(text: &str) -> usize {
    text.split_whitespace().count().max((text.len() + 3) / 4)
}

fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_round_trips_plain_names() {
        let name = ToolName::from_str("read_file").expect("known tool");
        assert_eq!(name, ToolName::ReadFile);
        assert_eq!(name.as_str(), "read_file");
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"read_file\"");
        assert_eq!(
            serde_json::from_str::<ToolName>("\"read_file\"").unwrap(),
            name
        );
    }

    #[test]
    fn file_change_preview_is_internal_and_not_serialized() {
        let request = ToolRequest {
            id: "edit-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: None,
            raw_arguments: None,
        };
        let result = ToolResult::completed(&request, "edited file.txt".to_string(), false)
            .with_file_change_preview(FileChangePreview::UnifiedDiff {
                text: "--- a/file.txt\n+++ b/file.txt".to_string(),
                truncated: false,
            });

        let value = serde_json::to_value(&result).expect("serialize tool result");
        assert!(value.get("file_change_preview").is_none());
        let restored: ToolResult = serde_json::from_value(value).expect("deserialize tool result");
        assert!(restored.file_change_preview.is_none());
    }

    #[test]
    fn tool_name_preserves_mcp_namespace() {
        let name = ToolName::from_str("mcp__foo__exec_command").expect("mcp tool");
        assert_eq!(name, ToolName::Mcp("mcp__foo__exec_command".to_string()));
        assert_eq!(name.namespace(), Some("mcp__foo"));
        assert_eq!(name.local_name(), "exec_command");
        assert_eq!(name.as_str(), "mcp__foo__exec_command");
    }

    #[test]
    fn tool_name_helpers_support_non_legacy_namespaced_tools() {
        let name = ToolName::namespaced("vendor", "inspect");
        assert_eq!(name, ToolName::namespaced("vendor", "inspect"));
        assert_eq!(name.namespace(), Some("vendor"));
        assert_eq!(name.local_name(), "inspect");
        assert_eq!(name.as_str(), "vendor__inspect");
    }

    #[test]
    fn capability_set_derives_action_kind() {
        assert_eq!(
            CapabilitySet::read_only_fs().action_kind(),
            ActionKind::Read
        );
        assert_eq!(
            CapabilitySet::filesystem_write().action_kind(),
            ActionKind::Write
        );
        assert_eq!(
            CapabilitySet::shell_execute().action_kind(),
            ActionKind::Shell
        );
        assert_eq!(
            CapabilitySet::network_search().action_kind(),
            ActionKind::Network
        );
        assert_eq!(
            CapabilitySet::agent_delegate().action_kind(),
            ActionKind::Agent
        );
    }

    #[test]
    fn empty_capability_set_is_not_read_only() {
        assert!(!CapabilitySet::new(Vec::new()).is_read_only());
    }

    #[test]
    fn completed_result_kinds_remain_completed_status() {
        assert_eq!(ToolResultKind::Success.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::Empty.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::NoMatches.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::Truncated.status(), ToolStatus::Completed);
        assert_eq!(ToolResultKind::InvalidInput.status(), ToolStatus::Failed);
        assert_eq!(ToolResultKind::RuntimeError.status(), ToolStatus::Failed);
    }

    #[test]
    fn tool_terminal_constructors_distinguish_cancelled_and_indeterminate() {
        let request = ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("sleep 30".to_string()),
            raw_arguments: Some(r#"{"command":"sleep 30"}"#.to_string()),
        };

        let cancelled = ToolResult::cancelled(&request, "turn interrupted", Some(130));
        assert_eq!(cancelled.status, ToolStatus::Cancelled);
        assert_eq!(cancelled.kind, ToolResultKind::Cancelled);
        assert_eq!(cancelled.error.as_deref(), Some("turn interrupted"));
        assert_eq!(cancelled.exit_code, Some(130));

        let not_started = ToolResult::cancelled_before_start(&request, "turn interrupted");
        assert_eq!(not_started.status, ToolStatus::Cancelled);
        assert_eq!(not_started.kind, ToolResultKind::Cancelled);
        assert_eq!(not_started.exit_code, None);
        assert_eq!(not_started.terminal().started, ToolInvocationStarted::No);
        assert!(
            not_started
                .error
                .as_deref()
                .is_some_and(|error| error.contains("not started"))
        );

        let indeterminate = ToolResult::indeterminate(&request, "missing terminal result");
        assert_eq!(indeterminate.status, ToolStatus::Indeterminate);
        assert_eq!(indeterminate.kind, ToolResultKind::Indeterminate);
        assert_eq!(
            indeterminate.error.as_deref(),
            Some("missing terminal result")
        );
        assert_eq!(ToolStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(ToolStatus::Indeterminate.as_str(), "indeterminate");

        let failed_after_start = ToolResult::failed_after_start(&request, "cleanup failed", None);
        assert_eq!(failed_after_start.status, ToolStatus::Failed);
        assert_eq!(
            failed_after_start.terminal().started,
            ToolInvocationStarted::Yes
        );
    }

    #[test]
    fn tool_terminal_metadata_serializes_with_stable_snake_case_names() {
        let terminal = ToolTerminal {
            status: ToolStatus::Indeterminate,
            error: Some("missing terminal result".to_string()),
            exit_code: None,
            truncated: false,
            kind: ToolResultKind::Indeterminate,
            source: ToolTerminalSource::CompatibilityRepair,
            started: ToolInvocationStarted::Unknown,
        };

        let value = serde_json::to_value(terminal).unwrap();
        assert_eq!(value["status"], "indeterminate");
        assert_eq!(value["kind"], "indeterminate");
        assert_eq!(value["terminal_source"], "compatibility_repair");
        assert!(value.get("started").is_none());
        assert_eq!(
            serde_json::to_value(InterruptSemantics::CooperativeCancel).unwrap(),
            "cooperative_cancel"
        );
        assert_eq!(
            serde_json::to_value(ReplaySemantics::IndeterminateAfterStart).unwrap(),
            "indeterminate_after_start"
        );

        let request = ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: None,
            raw_arguments: None,
        };
        let result = ToolResult::cancelled(&request, "turn interrupted", Some(130));
        assert_eq!(
            result.terminal().clone(),
            ToolTerminal {
                status: ToolStatus::Cancelled,
                error: Some("turn interrupted".to_string()),
                exit_code: Some(130),
                truncated: false,
                kind: ToolResultKind::Cancelled,
                source: ToolTerminalSource::Observed,
                started: ToolInvocationStarted::Yes,
            }
        );

        let legacy = serde_json::json!({
            "id": "call-legacy",
            "name": "bash",
            "status": "failed",
            "output": null,
            "error": "boom",
            "exit_code": 1,
            "truncated": false,
            "kind": "runtime_error"
        });
        let decoded: ToolResult = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(decoded.terminal().source, ToolTerminalSource::Observed);
        assert_eq!(decoded.terminal().started, ToolInvocationStarted::Unknown);
        assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);

        let before_start = ToolResult::cancelled_before_start(&request, "turn interrupted");
        let value = serde_json::to_value(&before_start).unwrap();
        assert_eq!(value["invocation_started"], "no");
        let decoded: ToolResult = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.terminal().started, ToolInvocationStarted::No);

        let contradictory = serde_json::json!({
            "id": "call-invalid",
            "name": "bash",
            "status": "cancelled",
            "output": null,
            "error": "cancelled",
            "exit_code": null,
            "truncated": false,
            "kind": "runtime_error"
        });
        assert!(serde_json::from_value::<ToolResult>(contradictory).is_err());
    }

    #[test]
    #[should_panic(expected = "completed_kind requires a completed result kind")]
    fn completed_kind_rejects_non_completion_kinds() {
        let request = ToolRequest {
            id: "call-1".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: None,
            raw_arguments: None,
        };
        let _ = ToolResult::completed_kind(
            &request,
            "not actually complete".to_string(),
            false,
            ToolResultKind::Cancelled,
        );
    }

    #[test]
    fn truncation_policy_bytes_keeps_existing_middle_compaction_marker() {
        let policy = ToolOutputTruncation::bytes(64);
        let text = "abcdefghijklmnopqrstuvwxyz".repeat(8);

        let (output, truncated) = truncate_output_with_policy(text, policy);

        assert!(truncated);
        assert!(output.contains("tool output micro-compacted"));
        assert!(output.starts_with("a"));
        assert!(output.ends_with("z"));
    }

    #[test]
    fn truncation_policy_tokens_reports_original_size_and_line_count() {
        let policy = ToolOutputTruncation::tokens(16);
        let text = format!(
            "{}\n{}",
            "alpha beta gamma delta epsilon zeta ".repeat(4),
            "eta theta iota kappa lambda zeta ".repeat(4)
        );

        let (output, truncated) = truncate_output_with_policy(text, policy);

        assert!(truncated);
        assert!(output.starts_with("Warning: truncated tool output"));
        assert!(output.contains("Original token count:"));
        assert!(output.contains("Original line count: 2"));
        assert!(output.contains("alpha"));
        assert!(output.contains("zeta"));
    }
}
