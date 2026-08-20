use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::server::{
    ActivePermissionProfile, AdditionalWorkingDirectory, PermissionProfileOverride,
    PermissionRuleValue, PermissionUpdate,
};
use crate::shell_session::ShellTerminalMode;
use crate::thread_store::{SortDirection, ThreadListFilters, ThreadSortKey, TurnItemsView};

use super::command_exec::{
    CommandEnvOverrides, CommandExecOptions, command_args_from_wire, command_cwd_from_wire,
    command_exec_options_from_params, command_is_argv_from_wire, command_text_from_wire,
};
use super::permissions::{
    PermissionGrantScope, PermissionResponseDecision, RequestPermissionProfile,
};
use super::shell::shell_terminal_mode_from_params;
use super::thread::{
    parse_items_view, parse_sort_direction_asc_default, parse_sort_direction_desc_default,
    parse_thread_list_filters, parse_thread_sort_key,
};
use super::turn::{prompt_from_turn_start_params, turn_start_input_from_params};

#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
    pub id: Value,
    pub op: ClientOp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientOp {
    Submit {
        thread_id: Option<String>,
        prompt: String,
        permissions: PermissionProfileOverride,
    },
    SubmitWithMentions {
        thread_id: Option<String>,
        prompt: String,
        bindings: crate::mentions::MentionBindings,
        permissions: PermissionProfileOverride,
    },
    ThreadStart {
        runtime_workspace_roots: Option<Vec<PathBuf>>,
    },
    ThreadResume {
        thread_id: String,
        permissions: PermissionProfileOverride,
    },
    ThreadFork {
        thread_id: String,
        permissions: PermissionProfileOverride,
    },
    ThreadRead {
        thread_id: String,
        include_messages: bool,
        include_turns: bool,
    },
    ThreadList {
        cursor: Option<String>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<String>,
        limit: usize,
        filters: ThreadListFilters,
    },
    ThreadSearch {
        query: String,
        cursor: Option<String>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        include_archived: bool,
        limit: usize,
    },
    ThreadTurnsList {
        thread_id: String,
        cursor: Option<String>,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
        limit: usize,
    },
    ThreadItemsList {
        thread_id: String,
        turn_id: Option<String>,
        cursor: Option<String>,
        sort_direction: SortDirection,
        limit: usize,
    },
    ThreadMetadataUpdate {
        thread_id: String,
        title: Option<String>,
    },
    ThreadQueue {
        thread_id: String,
        action: crate::prompt_queue::PromptQueueAction,
    },
    TurnInterrupt {
        thread_id: Option<String>,
        turn_id: String,
    },
    TurnResume {
        thread_id: Option<String>,
        turn_id: String,
    },
    TurnSteer {
        thread_id: Option<String>,
        turn_id: String,
        input: String,
    },
    PermissionRespond {
        request_id: String,
        decision: PermissionResponseDecision,
        scope: PermissionGrantScope,
        permissions: RequestPermissionProfile,
        strict_auto_review: bool,
    },
    UserInputRespond {
        request_id: String,
        answer: Option<String>,
    },
    McpElicitationRespond {
        request_id: String,
        accepted: bool,
        content_json: Option<Value>,
    },
    ShellStart {
        thread_id: Option<String>,
        command: String,
        description: Option<String>,
        terminal: crate::shell_session::ShellTerminalMode,
    },
    ShellCapabilities,
    ShellWrite {
        shell_id: String,
        input: String,
    },
    ShellUpdate {
        shell_id: String,
        description: Option<String>,
    },
    ShellClose {
        shell_id: String,
    },
    ShellResize {
        shell_id: String,
        cols: u16,
        rows: u16,
    },
    ShellList,
    ShellRead {
        shell_id: String,
        timeout_ms: u64,
        output_bytes_cap: Option<usize>,
    },
    ShellKill {
        shell_id: String,
    },
    CommandExec {
        thread_id: Option<String>,
        command: Vec<String>,
        command_is_argv: bool,
        process_id: Option<String>,
        cwd: Option<PathBuf>,
        env: CommandEnvOverrides,
        options: CommandExecOptions,
        terminal: crate::shell_session::ShellTerminalMode,
    },
    CommandExecList,
    CommandExecWrite {
        process_id: String,
        delta_base64: Option<String>,
        close_stdin: bool,
    },
    CommandExecRead {
        process_id: String,
        timeout_ms: u64,
        output_bytes_cap: Option<usize>,
    },
    CommandExecResize {
        process_id: String,
        cols: u16,
        rows: u16,
    },
    CommandExecTerminate {
        process_id: String,
    },
    FuzzyFileSearchSessionStart {
        session_id: String,
        roots: Vec<PathBuf>,
        exclude: Vec<String>,
        respect_gitignore: bool,
        result_limit: usize,
    },
    FuzzyFileSearchSessionUpdate {
        session_id: String,
        query: String,
    },
    FuzzyFileSearchSessionStop {
        session_id: String,
    },
    MentionSearchSessionStart {
        session_id: String,
        thread_id: String,
        exclude: Vec<String>,
        respect_gitignore: bool,
        result_limit: usize,
    },
    MentionSearchSessionUpdate {
        session_id: String,
        query: String,
    },
    MentionSearchSessionStop {
        session_id: String,
    },
}

#[derive(Debug, PartialEq)]
pub struct DecodeError {
    pub id: Value,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct WireSubmission {
    id: Value,
    op: Option<String>,
    method: Option<String>,
    prompt: Option<String>,
    params: Option<WireParams>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WireParams {
    #[serde(rename = "threadId")]
    pub(super) thread_id: Option<String>,
    #[serde(rename = "turnId", default)]
    pub(super) turn_id: Option<String>,
    #[serde(rename = "requestId", default)]
    pub(super) request_id: Option<String>,
    #[serde(default)]
    pub(super) decision: Option<PermissionResponseDecision>,
    #[serde(default)]
    pub(super) scope: Option<PermissionGrantScope>,
    #[serde(default)]
    pub(super) permissions: Option<RequestPermissionProfile>,
    #[serde(rename = "strictAutoReview", default)]
    pub(super) strict_auto_review: bool,
    #[serde(default)]
    pub(super) cursor: Option<String>,
    #[serde(rename = "includeMessages", default)]
    pub(super) include_messages: bool,
    #[serde(rename = "includeTurns", default)]
    pub(super) include_turns: bool,
    #[serde(default)]
    pub(super) limit: Option<usize>,
    #[serde(rename = "sortDirection", default)]
    pub(super) sort_direction: Option<String>,
    #[serde(rename = "sortKey", default)]
    pub(super) sort_key: Option<String>,
    #[serde(rename = "itemsView", default)]
    pub(super) items_view: Option<String>,
    #[serde(default)]
    pub(super) archived: Option<bool>,
    #[serde(rename = "modelProviders", default)]
    pub(super) model_providers: Option<Vec<String>>,
    #[serde(default)]
    pub(super) model: Option<CwdOrModelFilter>,
    #[serde(default)]
    pub(super) cwd: Option<CwdOrModelFilter>,
    #[serde(default)]
    pub(super) env: Option<CommandEnvOverrides>,
    #[serde(rename = "parentThreadId", default)]
    pub(super) parent_thread_id: Option<String>,
    #[serde(rename = "ancestorThreadId", default)]
    pub(super) ancestor_thread_id: Option<String>,
    #[serde(rename = "searchTerm", default)]
    pub(super) search_term: Option<String>,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(rename = "expectedRevision", default)]
    pub(super) expected_revision: Option<u64>,
    #[serde(rename = "queueId", default)]
    pub(super) queue_id: Option<String>,
    #[serde(rename = "orderedIds", default)]
    pub(super) ordered_ids: Vec<String>,
    #[serde(rename = "shellId", default)]
    pub(super) shell_id: Option<String>,
    #[serde(default)]
    pub(super) command: Option<WireCommandParam>,
    #[serde(rename = "processId", default)]
    pub(super) process_id: Option<String>,
    #[serde(rename = "deltaBase64", default)]
    pub(super) delta_base64: Option<String>,
    #[serde(rename = "closeStdin", default)]
    pub(super) close_stdin: bool,
    #[serde(default)]
    pub(super) size: Option<WireTerminalSize>,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) pty: bool,
    #[serde(default)]
    pub(super) tty: bool,
    #[serde(rename = "terminalMode", default)]
    pub(super) terminal_mode: Option<String>,
    #[serde(default)]
    pub(super) cols: Option<u16>,
    #[serde(default)]
    pub(super) rows: Option<u16>,
    #[serde(default)]
    pub(super) input: Option<WireInputParam>,
    #[serde(default)]
    pub(super) answer: Option<String>,
    #[serde(default)]
    pub(super) accepted: bool,
    #[serde(rename = "contentJson", default)]
    pub(super) content_json: Option<Value>,
    #[serde(rename = "timeoutMs", default)]
    pub(super) timeout_ms: Option<i64>,
    #[serde(rename = "streamStdin", default)]
    pub(super) stream_stdin: bool,
    #[serde(rename = "streamStdoutStderr", default)]
    pub(super) stream_stdout_stderr: bool,
    #[serde(rename = "outputBytesCap", default)]
    pub(super) output_bytes_cap: Option<u64>,
    #[serde(rename = "disableOutputCap", default)]
    pub(super) disable_output_cap: bool,
    #[serde(rename = "disableTimeout", default)]
    pub(super) disable_timeout: bool,
    #[serde(rename = "sandboxPolicy", default)]
    pub(super) sandbox_policy: Option<Value>,
    #[serde(rename = "permissionProfile", default)]
    pub(super) permission_profile: Option<String>,
    #[serde(rename = "approvalMode", default)]
    approval_mode: Option<orca_core::approval_types::ApprovalMode>,
    #[serde(rename = "approvalPolicy", default)]
    approval_policy: Option<AppServerApprovalPolicy>,
    #[serde(rename = "activePermissionProfile", default)]
    active_permission_profile: Option<ActivePermissionProfile>,
    #[serde(rename = "permissionRules", default)]
    permission_rules: Option<orca_core::approval_rules::PermissionRules>,
    #[serde(rename = "permissionUpdates", default)]
    permission_updates: Vec<WirePermissionUpdate>,
    #[serde(rename = "runtimeWorkspaceRoots", default)]
    runtime_workspace_roots: Option<Vec<PathBuf>>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    #[serde(default)]
    roots: Option<Vec<PathBuf>>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(rename = "respectGitignore", default = "default_true")]
    respect_gitignore: bool,
    #[serde(rename = "resultLimit", default)]
    result_limit: Option<usize>,
}

impl Default for WireParams {
    fn default() -> Self {
        serde_json::from_value(Value::Object(Default::default()))
            .expect("empty JSON object is valid wire params")
    }
}

fn default_true() -> bool {
    true
}

fn required_queue_revision(
    id: &Value,
    value: Option<u64>,
) -> Result<crate::prompt_queue::QueueRevision, DecodeError> {
    value
        .map(crate::prompt_queue::QueueRevision::from_u64)
        .ok_or_else(|| DecodeError {
            id: id.clone(),
            message: "thread/queue mutation params.expectedRevision is required".to_string(),
        })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum CwdOrModelFilter {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum WireUserInput {
    Text {
        text: String,
    },
    Image {},
    LocalImage {},
    Skill {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        path: Option<PathBuf>,
    },
    Mention {
        name: String,
        target: crate::mentions::MentionTarget,
        #[serde(default)]
        start: Option<usize>,
        #[serde(default)]
        end: Option<usize>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum WireInputParam {
    Items(Vec<WireUserInput>),
    Text(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum WireCommandParam {
    Text(String),
    Args(Vec<String>),
}

#[derive(Clone, Debug, Deserialize)]
pub(super) struct WireTerminalSize {
    pub(super) cols: u16,
    pub(super) rows: u16,
}

impl Submission {
    pub fn decode(line: &str) -> Result<Self, DecodeError> {
        let wire = serde_json::from_str::<WireSubmission>(line).map_err(|error| DecodeError {
            id: Value::Null,
            message: format!("invalid request: {error}"),
        })?;
        match (wire.op.as_deref(), wire.method.as_deref()) {
            (Some("submit"), _) => Ok(Self {
                id: wire.id,
                op: ClientOp::Submit {
                    thread_id: None,
                    prompt: wire.prompt.unwrap_or_default(),
                    permissions: PermissionProfileOverride::default(),
                },
            }),
            (_, Some("thread/start")) => {
                let runtime_workspace_roots = wire
                    .params
                    .as_ref()
                    .and_then(|params| params.runtime_workspace_roots.clone())
                    .map(normalize_runtime_workspace_roots);
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::ThreadStart {
                        runtime_workspace_roots,
                    },
                })
            }
            (_, Some("fuzzyFileSearch/sessionStart")) => Ok(Self {
                id: wire.id,
                op: ClientOp::FuzzyFileSearchSessionStart {
                    session_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.session_id.clone())
                        .unwrap_or_default(),
                    roots: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.roots.clone())
                        .map(normalize_runtime_workspace_roots)
                        .unwrap_or_default(),
                    exclude: wire
                        .params
                        .as_ref()
                        .map(|params| params.exclude.clone())
                        .unwrap_or_default(),
                    respect_gitignore: wire
                        .params
                        .as_ref()
                        .is_none_or(|params| params.respect_gitignore),
                    result_limit: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.result_limit)
                        .unwrap_or(12),
                },
            }),
            (_, Some("fuzzyFileSearch/sessionUpdate")) => Ok(Self {
                id: wire.id,
                op: ClientOp::FuzzyFileSearchSessionUpdate {
                    session_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.session_id.clone())
                        .unwrap_or_default(),
                    query: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.query.clone())
                        .unwrap_or_default(),
                },
            }),
            (_, Some("fuzzyFileSearch/sessionStop")) => Ok(Self {
                id: wire.id,
                op: ClientOp::FuzzyFileSearchSessionStop {
                    session_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.session_id.clone())
                        .unwrap_or_default(),
                },
            }),
            (_, Some("mention/search/start")) => Ok(Self {
                id: wire.id,
                op: ClientOp::MentionSearchSessionStart {
                    session_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.session_id.clone())
                        .unwrap_or_default(),
                    thread_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.thread_id.clone())
                        .unwrap_or_default(),
                    exclude: wire
                        .params
                        .as_ref()
                        .map(|params| params.exclude.clone())
                        .unwrap_or_default(),
                    respect_gitignore: wire
                        .params
                        .as_ref()
                        .is_none_or(|params| params.respect_gitignore),
                    result_limit: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.result_limit)
                        .unwrap_or(12),
                },
            }),
            (_, Some("mention/search/update")) => Ok(Self {
                id: wire.id,
                op: ClientOp::MentionSearchSessionUpdate {
                    session_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.session_id.clone())
                        .unwrap_or_default(),
                    query: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.query.clone())
                        .unwrap_or_default(),
                },
            }),
            (_, Some("mention/search/stop")) => Ok(Self {
                id: wire.id,
                op: ClientOp::MentionSearchSessionStop {
                    session_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.session_id.clone())
                        .unwrap_or_default(),
                },
            }),
            (_, Some("thread/resume")) => {
                let thread_id = wire
                    .params
                    .as_ref()
                    .and_then(|params| params.thread_id.clone())
                    .unwrap_or_default();
                let permissions = wire
                    .params
                    .as_ref()
                    .map(permission_profile_override)
                    .unwrap_or_default();
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::ThreadResume {
                        thread_id,
                        permissions,
                    },
                })
            }
            (_, Some("thread/fork")) => {
                let thread_id = wire
                    .params
                    .as_ref()
                    .and_then(|params| params.thread_id.clone())
                    .unwrap_or_default();
                let permissions = wire
                    .params
                    .as_ref()
                    .map(permission_profile_override)
                    .unwrap_or_default();
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::ThreadFork {
                        thread_id,
                        permissions,
                    },
                })
            }
            (_, Some("thread/read")) => {
                let thread_id = wire
                    .params
                    .as_ref()
                    .and_then(|params| params.thread_id.clone())
                    .unwrap_or_default();
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::ThreadRead {
                        thread_id,
                        include_messages: wire
                            .params
                            .as_ref()
                            .map(|params| params.include_messages)
                            .unwrap_or(false),
                        include_turns: wire
                            .params
                            .as_ref()
                            .map(|params| params.include_turns)
                            .unwrap_or(false),
                    },
                })
            }
            (_, Some("thread/list")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ThreadList {
                    cursor: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.cursor.clone()),
                    sort_direction: wire
                        .params
                        .as_ref()
                        .map(parse_sort_direction_desc_default)
                        .unwrap_or(SortDirection::Desc),
                    sort_key: wire
                        .params
                        .as_ref()
                        .map(parse_thread_sort_key)
                        .unwrap_or(ThreadSortKey::UpdatedAt),
                    search_term: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.search_term.clone()),
                    limit: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.limit)
                        .unwrap_or(50),
                    filters: wire
                        .params
                        .as_ref()
                        .map(parse_thread_list_filters)
                        .unwrap_or_default(),
                },
            }),
            (_, Some("thread/search")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ThreadSearch {
                    query: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.search_term.clone())
                        .unwrap_or_default(),
                    cursor: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.cursor.clone()),
                    sort_key: wire
                        .params
                        .as_ref()
                        .map(parse_thread_sort_key)
                        .unwrap_or(ThreadSortKey::UpdatedAt),
                    sort_direction: wire
                        .params
                        .as_ref()
                        .map(parse_sort_direction_desc_default)
                        .unwrap_or(SortDirection::Desc),
                    include_archived: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.archived)
                        .unwrap_or(false),
                    limit: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.limit)
                        .unwrap_or(50),
                },
            }),
            (_, Some("thread/turns/list")) => {
                let thread_id = wire
                    .params
                    .as_ref()
                    .and_then(|params| params.thread_id.clone())
                    .unwrap_or_default();
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::ThreadTurnsList {
                        thread_id,
                        cursor: wire
                            .params
                            .as_ref()
                            .and_then(|params| params.cursor.clone()),
                        sort_direction: wire
                            .params
                            .as_ref()
                            .map(parse_sort_direction_asc_default)
                            .unwrap_or(SortDirection::Asc),
                        items_view: wire
                            .params
                            .as_ref()
                            .map(parse_items_view)
                            .unwrap_or(TurnItemsView::Full),
                        limit: wire
                            .params
                            .as_ref()
                            .and_then(|params| params.limit)
                            .unwrap_or(50),
                    },
                })
            }
            (_, Some("thread/items/list")) => {
                let thread_id = wire
                    .params
                    .as_ref()
                    .and_then(|params| params.thread_id.clone())
                    .unwrap_or_default();
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::ThreadItemsList {
                        thread_id,
                        turn_id: wire
                            .params
                            .as_ref()
                            .and_then(|params| params.turn_id.clone()),
                        cursor: wire
                            .params
                            .as_ref()
                            .and_then(|params| params.cursor.clone()),
                        sort_direction: wire
                            .params
                            .as_ref()
                            .map(parse_sort_direction_asc_default)
                            .unwrap_or(SortDirection::Asc),
                        limit: wire
                            .params
                            .as_ref()
                            .and_then(|params| params.limit)
                            .unwrap_or(50),
                    },
                })
            }
            (_, Some("thread/metadata/update")) => {
                let thread_id = wire
                    .params
                    .as_ref()
                    .and_then(|params| params.thread_id.clone())
                    .unwrap_or_default();
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::ThreadMetadataUpdate {
                        thread_id,
                        title: wire.params.and_then(|params| params.title),
                    },
                })
            }
            (
                _,
                Some(
                    method @ ("thread/queue/list"
                    | "thread/queue/add"
                    | "thread/queue/update"
                    | "thread/queue/delete"
                    | "thread/queue/reorder"
                    | "thread/queue/pause"
                    | "thread/queue/start"),
                ),
            ) => {
                let params = wire.params.unwrap_or_default();
                let parse_id = |value: Option<String>| {
                    crate::prompt_queue::QueuedSubmissionId::parse(value.unwrap_or_default())
                        .map_err(|message| DecodeError {
                            id: wire.id.clone(),
                            message,
                        })
                };
                let action = match method {
                    "thread/queue/list" => crate::prompt_queue::PromptQueueAction::List,
                    "thread/queue/add" => crate::prompt_queue::PromptQueueAction::Add {
                        input: match params.input {
                            Some(WireInputParam::Text(value)) => value.into(),
                            _ => crate::prompt_queue::PromptQueueInput::text(""),
                        },
                    },
                    "thread/queue/update" => crate::prompt_queue::PromptQueueAction::Update {
                        expected_revision: required_queue_revision(
                            &wire.id,
                            params.expected_revision,
                        )?,
                        id: parse_id(params.queue_id)?,
                        input: match params.input {
                            Some(WireInputParam::Text(value)) => value.into(),
                            _ => crate::prompt_queue::PromptQueueInput::text(""),
                        },
                    },
                    "thread/queue/delete" => crate::prompt_queue::PromptQueueAction::Delete {
                        expected_revision: required_queue_revision(
                            &wire.id,
                            params.expected_revision,
                        )?,
                        id: parse_id(params.queue_id)?,
                    },
                    "thread/queue/reorder" => crate::prompt_queue::PromptQueueAction::Reorder {
                        expected_revision: required_queue_revision(
                            &wire.id,
                            params.expected_revision,
                        )?,
                        ordered_ids: params
                            .ordered_ids
                            .into_iter()
                            .map(crate::prompt_queue::QueuedSubmissionId::parse)
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|message| DecodeError {
                                id: wire.id.clone(),
                                message,
                            })?,
                    },
                    "thread/queue/pause" => crate::prompt_queue::PromptQueueAction::Pause {
                        expected_revision: required_queue_revision(
                            &wire.id,
                            params.expected_revision,
                        )?,
                    },
                    "thread/queue/start" => crate::prompt_queue::PromptQueueAction::Start {
                        expected_revision: required_queue_revision(
                            &wire.id,
                            params.expected_revision,
                        )?,
                    },
                    _ => unreachable!(),
                };
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::ThreadQueue {
                        thread_id: params.thread_id.unwrap_or_default(),
                        action,
                    },
                })
            }
            (_, Some("turn/interrupt")) => Ok(Self {
                id: wire.id,
                op: ClientOp::TurnInterrupt {
                    thread_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.thread_id.clone()),
                    turn_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.turn_id.clone())
                        .unwrap_or_default(),
                },
            }),
            (_, Some("turn/resume")) => Ok(Self {
                id: wire.id,
                op: ClientOp::TurnResume {
                    thread_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.thread_id.clone()),
                    turn_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.turn_id.clone())
                        .unwrap_or_default(),
                },
            }),
            (_, Some("turn/steer")) => Ok(Self {
                id: wire.id,
                op: ClientOp::TurnSteer {
                    thread_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.thread_id.clone()),
                    turn_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.turn_id.clone())
                        .unwrap_or_default(),
                    input: prompt_from_turn_start_params(wire.params),
                },
            }),
            (_, Some("permission/respond")) => {
                let params = wire.params.as_ref();
                let Some(request_id) = params
                    .and_then(|params| params.request_id.clone())
                    .filter(|request_id| !request_id.is_empty())
                else {
                    return Err(DecodeError {
                        id: wire.id,
                        message: "permission/respond params.requestId is required".to_string(),
                    });
                };
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::PermissionRespond {
                        request_id,
                        decision: params
                            .and_then(|params| params.decision)
                            .unwrap_or(PermissionResponseDecision::Deny),
                        scope: params.and_then(|params| params.scope).unwrap_or_default(),
                        permissions: params
                            .and_then(|params| params.permissions.clone())
                            .unwrap_or_default()
                            .normalize_file_system_entries(),
                        strict_auto_review: params
                            .map(|params| params.strict_auto_review)
                            .unwrap_or(false),
                    },
                })
            }
            (_, Some("user_input/respond")) => {
                let params = wire.params.as_ref();
                let Some(request_id) = params
                    .and_then(|params| params.request_id.clone())
                    .filter(|request_id| !request_id.is_empty())
                else {
                    return Err(DecodeError {
                        id: wire.id,
                        message: "user_input/respond params.requestId is required".to_string(),
                    });
                };
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::UserInputRespond {
                        request_id,
                        answer: params.and_then(|params| params.answer.clone()),
                    },
                })
            }
            (_, Some("mcp_elicitation/respond")) => {
                let params = wire.params.as_ref();
                let Some(request_id) = params
                    .and_then(|params| params.request_id.clone())
                    .filter(|request_id| !request_id.is_empty())
                else {
                    return Err(DecodeError {
                        id: wire.id,
                        message: "mcp_elicitation/respond params.requestId is required".to_string(),
                    });
                };
                Ok(Self {
                    id: wire.id,
                    op: ClientOp::McpElicitationRespond {
                        request_id,
                        accepted: params.map(|params| params.accepted).unwrap_or(false),
                        content_json: params.and_then(|params| params.content_json.clone()),
                    },
                })
            }
            (_, Some("shell/start")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ShellStart {
                    thread_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.thread_id.clone()),
                    command: wire
                        .params
                        .as_ref()
                        .and_then(|params| command_text_from_wire(params.command.as_ref()))
                        .unwrap_or_default(),
                    description: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.description.clone()),
                    terminal: wire
                        .params
                        .as_ref()
                        .map(shell_terminal_mode_from_params)
                        .unwrap_or_else(ShellTerminalMode::pipe),
                },
            }),
            (_, Some("shell/capabilities")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ShellCapabilities,
            }),
            (_, Some("command/exec")) => Ok(Self {
                id: wire.id,
                op: ClientOp::CommandExec {
                    thread_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.thread_id.clone()),
                    command: wire
                        .params
                        .as_ref()
                        .and_then(|params| command_args_from_wire(params.command.as_ref()))
                        .unwrap_or_default(),
                    command_is_argv: wire
                        .params
                        .as_ref()
                        .is_some_and(|params| command_is_argv_from_wire(params.command.as_ref())),
                    process_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.process_id.clone()),
                    cwd: wire
                        .params
                        .as_ref()
                        .and_then(|params| command_cwd_from_wire(params.cwd.as_ref())),
                    env: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.env.clone())
                        .unwrap_or_default(),
                    options: wire
                        .params
                        .as_ref()
                        .map(command_exec_options_from_params)
                        .unwrap_or_default(),
                    terminal: wire
                        .params
                        .as_ref()
                        .map(shell_terminal_mode_from_params)
                        .unwrap_or_else(ShellTerminalMode::pipe),
                },
            }),
            (_, Some("command/exec/list")) => Ok(Self {
                id: wire.id,
                op: ClientOp::CommandExecList,
            }),
            (_, Some("command/exec/write")) => Ok(Self {
                id: wire.id,
                op: ClientOp::CommandExecWrite {
                    process_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.process_id.clone())
                        .unwrap_or_default(),
                    delta_base64: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.delta_base64.clone()),
                    close_stdin: wire
                        .params
                        .as_ref()
                        .map(|params| params.close_stdin)
                        .unwrap_or(false),
                },
            }),
            (_, Some("command/exec/read")) => Ok(Self {
                id: wire.id,
                op: ClientOp::CommandExecRead {
                    process_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.process_id.clone())
                        .unwrap_or_default(),
                    timeout_ms: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.timeout_ms)
                        .and_then(|timeout_ms| u64::try_from(timeout_ms).ok())
                        .unwrap_or(100),
                    output_bytes_cap: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.output_bytes_cap)
                        .and_then(|cap| usize::try_from(cap).ok()),
                },
            }),
            (_, Some("command/exec/resize")) => Ok(Self {
                id: wire.id,
                op: ClientOp::CommandExecResize {
                    process_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.process_id.clone())
                        .unwrap_or_default(),
                    cols: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.size.as_ref().map(|size| size.cols))
                        .or_else(|| wire.params.as_ref().and_then(|params| params.cols))
                        .unwrap_or_default(),
                    rows: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.size.as_ref().map(|size| size.rows))
                        .or_else(|| wire.params.as_ref().and_then(|params| params.rows))
                        .unwrap_or_default(),
                },
            }),
            (_, Some("command/exec/terminate")) => Ok(Self {
                id: wire.id,
                op: ClientOp::CommandExecTerminate {
                    process_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.process_id.clone())
                        .unwrap_or_default(),
                },
            }),
            (_, Some("shell/write")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ShellWrite {
                    shell_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.shell_id.clone())
                        .unwrap_or_default(),
                    input: wire
                        .params
                        .as_ref()
                        .and_then(|params| match &params.input {
                            Some(WireInputParam::Text(input)) => Some(input.clone()),
                            _ => None,
                        })
                        .unwrap_or_default(),
                },
            }),
            (_, Some("shell/update")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ShellUpdate {
                    shell_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.shell_id.clone())
                        .unwrap_or_default(),
                    description: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.description.clone()),
                },
            }),
            (_, Some("shell/close")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ShellClose {
                    shell_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.shell_id.clone())
                        .unwrap_or_default(),
                },
            }),
            (_, Some("shell/resize")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ShellResize {
                    shell_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.shell_id.clone())
                        .unwrap_or_default(),
                    cols: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.cols)
                        .unwrap_or_default(),
                    rows: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.rows)
                        .unwrap_or_default(),
                },
            }),
            (_, Some("shell/list")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ShellList,
            }),
            (_, Some("shell/read")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ShellRead {
                    shell_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.shell_id.clone())
                        .unwrap_or_default(),
                    timeout_ms: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.timeout_ms)
                        .and_then(|timeout_ms| u64::try_from(timeout_ms).ok())
                        .unwrap_or(120_000),
                    output_bytes_cap: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.output_bytes_cap)
                        .and_then(|cap| usize::try_from(cap).ok()),
                },
            }),
            (_, Some("shell/kill")) => Ok(Self {
                id: wire.id,
                op: ClientOp::ShellKill {
                    shell_id: wire
                        .params
                        .as_ref()
                        .and_then(|params| params.shell_id.clone())
                        .unwrap_or_default(),
                },
            }),
            (_, Some("turn/start")) => {
                let params = wire.params;
                let permissions = params
                    .as_ref()
                    .map(permission_profile_override)
                    .unwrap_or_default();
                let thread_id = params.as_ref().and_then(|params| params.thread_id.clone());
                let input = turn_start_input_from_params(params);
                Ok(Self {
                    id: wire.id,
                    op: if input.bindings.is_empty() {
                        ClientOp::Submit {
                            thread_id,
                            prompt: input.prompt,
                            permissions,
                        }
                    } else {
                        ClientOp::SubmitWithMentions {
                            thread_id,
                            prompt: input.prompt,
                            bindings: input.bindings,
                            permissions,
                        }
                    },
                })
            }
            (Some(op), _) => Err(DecodeError {
                id: wire.id,
                message: format!("unsupported op: {op}"),
            }),
            (_, Some(method)) => Err(DecodeError {
                id: wire.id,
                message: format!("unsupported method: {method}"),
            }),
            (None, None) => Err(DecodeError {
                id: wire.id,
                message: "missing op or method".to_string(),
            }),
        }
    }
}

fn permission_profile_override(params: &WireParams) -> PermissionProfileOverride {
    PermissionProfileOverride {
        active_permission_profile: params.active_permission_profile.clone(),
        approval_mode: params
            .approval_mode
            .or_else(|| params.approval_policy.as_ref().map(|policy| policy.mode)),
        runtime_workspace_roots: params
            .runtime_workspace_roots
            .clone()
            .map(normalize_runtime_workspace_roots),
        permission_rules: params.permission_rules.clone(),
        permission_updates: params
            .permission_updates
            .iter()
            .filter_map(wire_permission_update)
            .collect(),
    }
}

fn normalize_runtime_workspace_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots
        .into_iter()
        .filter(|root| root.is_absolute())
        .fold(Vec::new(), |mut unique, root| {
            let normalized = normalize_path_components(root);
            if !unique.contains(&normalized) {
                unique.push(normalized);
            }
            unique
        })
}

fn normalize_path_components(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR.to_string()),
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    normalized
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum WirePermissionUpdate {
    AddRules {
        destination: String,
        behavior: WirePermissionBehavior,
        rules: Vec<WirePermissionRuleValue>,
    },
    ReplaceRules {
        destination: String,
        behavior: WirePermissionBehavior,
        rules: Vec<WirePermissionRuleValue>,
    },
    RemoveRules {
        destination: String,
        behavior: WirePermissionBehavior,
        rules: Vec<WirePermissionRuleValue>,
    },
    SetMode {
        destination: String,
        mode: WirePermissionMode,
    },
    AddDirectories {
        directories: Vec<PathBuf>,
        destination: String,
    },
    RemoveDirectories {
        directories: Vec<PathBuf>,
        destination: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum WirePermissionBehavior {
    Allow,
    Deny,
    Ask,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePermissionRuleValue {
    tool_name: String,
    rule_content: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum WirePermissionMode {
    AcceptEdits,
    BypassPermissions,
    Default,
    DontAsk,
    Plan,
}

fn wire_permission_update(update: &WirePermissionUpdate) -> Option<PermissionUpdate> {
    match update {
        WirePermissionUpdate::AddRules {
            destination,
            behavior,
            rules,
        } => Some(PermissionUpdate::AddRules {
            destination: destination.clone(),
            behavior: wire_permission_behavior(*behavior),
            rules: wire_permission_rules(rules),
        }),
        WirePermissionUpdate::ReplaceRules {
            destination,
            behavior,
            rules,
        } => Some(PermissionUpdate::ReplaceRules {
            destination: destination.clone(),
            behavior: wire_permission_behavior(*behavior),
            rules: wire_permission_rules(rules),
        }),
        WirePermissionUpdate::RemoveRules {
            destination,
            behavior,
            rules,
        } => Some(PermissionUpdate::RemoveRules {
            destination: destination.clone(),
            behavior: wire_permission_behavior(*behavior),
            rules: wire_permission_rules(rules),
        }),
        WirePermissionUpdate::SetMode { destination, mode } => Some(PermissionUpdate::SetMode {
            destination: destination.clone(),
            mode: wire_permission_mode(*mode),
        }),
        WirePermissionUpdate::AddDirectories {
            directories,
            destination,
        } => Some(PermissionUpdate::AddDirectories {
            directories: directories
                .iter()
                .map(|directory| {
                    AdditionalWorkingDirectory::new(directory.clone(), destination.clone())
                })
                .collect(),
        }),
        WirePermissionUpdate::RemoveDirectories {
            directories,
            destination,
        } => Some(PermissionUpdate::RemoveDirectories {
            destination: destination.clone(),
            directories: directories.clone(),
        }),
    }
}

fn wire_permission_rules(rules: &[WirePermissionRuleValue]) -> Vec<PermissionRuleValue> {
    rules
        .iter()
        .map(|rule| {
            PermissionRuleValue::new(
                normalize_package3_tool_name(&rule.tool_name),
                rule.rule_content.clone(),
            )
        })
        .collect()
}

fn wire_permission_behavior(
    behavior: WirePermissionBehavior,
) -> orca_core::approval_types::Decision {
    match behavior {
        WirePermissionBehavior::Allow => orca_core::approval_types::Decision::Allow,
        WirePermissionBehavior::Deny => orca_core::approval_types::Decision::Deny,
        WirePermissionBehavior::Ask => orca_core::approval_types::Decision::Prompt,
    }
}

fn wire_permission_mode(mode: WirePermissionMode) -> orca_core::approval_types::ApprovalMode {
    match mode {
        WirePermissionMode::AcceptEdits => orca_core::approval_types::ApprovalMode::AutoEdit,
        WirePermissionMode::BypassPermissions | WirePermissionMode::DontAsk => {
            orca_core::approval_types::ApprovalMode::FullAuto
        }
        WirePermissionMode::Default => orca_core::approval_types::ApprovalMode::Suggest,
        WirePermissionMode::Plan => orca_core::approval_types::ApprovalMode::Plan,
    }
}

fn normalize_package3_tool_name(tool_name: &str) -> String {
    match tool_name {
        "Bash" => "bash".to_string(),
        "Read" => "read_file".to_string(),
        "Write" => "write_file".to_string(),
        "Edit" => "edit".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

#[derive(Clone, Debug, PartialEq)]
struct AppServerApprovalPolicy {
    mode: orca_core::approval_types::ApprovalMode,
}

impl<'de> Deserialize<'de> for AppServerApprovalPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let mode = match value {
            Value::String(value) => parse_app_server_approval_policy(&value),
            Value::Object(map) => map
                .get("granular")
                .map(|_| orca_core::approval_types::ApprovalMode::Suggest)
                .ok_or_else(|| {
                    serde::de::Error::custom("unsupported approvalPolicy object shape")
                })?,
            _ => {
                return Err(serde::de::Error::custom(
                    "approvalPolicy must be a string or granular object",
                ));
            }
        };
        Ok(Self { mode })
    }
}

fn parse_app_server_approval_policy(value: &str) -> orca_core::approval_types::ApprovalMode {
    match value {
        "never" => orca_core::approval_types::ApprovalMode::FullAuto,
        "on-request" | "untrusted" => orca_core::approval_types::ApprovalMode::Suggest,
        "suggest" => orca_core::approval_types::ApprovalMode::Suggest,
        "auto-edit" => orca_core::approval_types::ApprovalMode::AutoEdit,
        "full-auto" => orca_core::approval_types::ApprovalMode::FullAuto,
        "plan" => orca_core::approval_types::ApprovalMode::Plan,
        _ => orca_core::approval_types::ApprovalMode::Suggest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::protocol::{
        CommandSandboxPolicy, NetworkAccess, RequestFileSystemPermissions, ServerEvent,
        legacy_json_event, map_runtime_event_line,
    };
    use crate::thread_store::ThreadRelationFilter;
    use serde_json::json;

    fn test_absolute_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn test_protocol_path(value: &str) -> PathBuf {
        PathBuf::from(value)
    }

    #[test]
    fn submission_decodes_submit_wire_shape() {
        let submission =
            Submission::decode(r#"{"id":1,"op":"submit","prompt":"hello"}"#).expect("submission");

        assert_eq!(submission.id, Value::from(1));
        assert_eq!(
            submission.op,
            ClientOp::Submit {
                thread_id: None,
                prompt: "hello".to_string(),
                permissions: PermissionProfileOverride::default()
            }
        );
    }

    #[test]
    fn submission_decodes_turn_start_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"req-1","method":"turn/start","params":{"input":[{"type":"text","text":"hello from method"}]}}"#,
        )
        .expect("submission");

        assert_eq!(submission.id, Value::from("req-1"));
        assert_eq!(
            submission.op,
            ClientOp::Submit {
                thread_id: None,
                prompt: "hello from method".to_string(),
                permissions: PermissionProfileOverride::default()
            }
        );
    }

    #[test]
    fn submission_decodes_turn_start_approval_policy_override() {
        let submission = Submission::decode(
            r#"{"id":"req-1","method":"turn/start","params":{"threadId":"thread-1","approvalPolicy":"never","activePermissionProfile":{"id":"locked-down","extends":":workspace"},"permissionRules":{"rules":[{"tool":"bash","pattern":"cargo test *","decision":"prompt"}]},"input":[{"type":"text","text":"hello"}]}}"#,
        )
        .expect("submission");

        match submission.op {
            ClientOp::Submit {
                thread_id,
                prompt,
                permissions,
            } => {
                assert_eq!(thread_id, Some("thread-1".to_string()));
                assert_eq!(prompt, "hello");
                assert_eq!(
                    permissions.approval_mode,
                    Some(orca_core::approval_types::ApprovalMode::FullAuto)
                );
                assert_eq!(
                    permissions.active_permission_profile,
                    Some(crate::server::ActivePermissionProfile::new(
                        "locked-down",
                        Some(":workspace")
                    ))
                );
                let rules = permissions.permission_rules.expect("permission rules");
                assert_eq!(rules.rules.len(), 1);
                assert_eq!(rules.rules[0].pattern, "cargo test *");
            }
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn submission_decodes_package3_permission_updates() {
        let submission = Submission::decode(
            r#"{"id":"req-1","method":"turn/start","params":{"threadId":"thread-1","permissionUpdates":[{"type":"setMode","mode":"bypassPermissions","destination":"session"},{"type":"addRules","behavior":"allow","destination":"session","rules":[{"toolName":"Bash","ruleContent":"cargo test *"}]},{"type":"removeRules","behavior":"deny","destination":"session","rules":[{"toolName":"Bash","ruleContent":"rm -rf *"}]},{"type":"replaceRules","behavior":"ask","destination":"session","rules":[{"toolName":"Write","ruleContent":"/workspace/**"}]},{"type":"addDirectories","destination":"projectSettings","directories":["/tmp/extra"]},{"type":"removeDirectories","destination":"session","directories":["/tmp/old"]}],"input":[{"type":"text","text":"hello"}]}}"#,
        )
        .expect("submission");

        match submission.op {
            ClientOp::Submit { permissions, .. } => {
                assert_eq!(permissions.permission_updates.len(), 6);
                assert_eq!(
                    permissions.permission_updates[0],
                    crate::server::PermissionUpdate::SetMode {
                        destination: "session".to_string(),
                        mode: orca_core::approval_types::ApprovalMode::FullAuto
                    }
                );
                assert_eq!(
                    permissions.permission_updates[1],
                    crate::server::PermissionUpdate::AddRules {
                        destination: "session".to_string(),
                        behavior: orca_core::approval_types::Decision::Allow,
                        rules: vec![crate::server::PermissionRuleValue::new(
                            "bash",
                            Some("cargo test *")
                        )]
                    }
                );
                assert_eq!(
                    permissions.permission_updates[3],
                    crate::server::PermissionUpdate::ReplaceRules {
                        destination: "session".to_string(),
                        behavior: orca_core::approval_types::Decision::Prompt,
                        rules: vec![crate::server::PermissionRuleValue::new(
                            "write_file",
                            Some("/workspace/**")
                        )]
                    }
                );
                assert_eq!(
                    permissions.permission_updates[4],
                    crate::server::PermissionUpdate::AddDirectories {
                        directories: vec![crate::server::AdditionalWorkingDirectory::new(
                            "/tmp/extra",
                            "projectSettings"
                        )]
                    }
                );
                assert_eq!(
                    permissions.permission_updates[5],
                    crate::server::PermissionUpdate::RemoveDirectories {
                        destination: "session".to_string(),
                        directories: vec![test_protocol_path("/tmp/old")]
                    }
                );
            }
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn submission_decodes_codex_special_file_system_entries() {
        let submission = Submission::decode(
            r#"{"id":"permission-response","method":"permission/respond","params":{"requestId":"permission-turn-1-request","decision":"allow","scope":"session","permissions":{"fileSystem":{"read":null,"write":null,"entries":[{"path":{"type":"special","value":{"kind":"project_roots","subpath":"docs"}},"access":"write"}]},"network":null}}}"#,
        )
        .expect("submission");

        match submission.op {
            ClientOp::PermissionRespond { permissions, .. } => {
                let file_system = permissions.file_system.expect("file system permissions");
                assert_eq!(
                    file_system.write,
                    Some(vec![PathBuf::from(":workspace_roots/docs")])
                );
                assert_eq!(file_system.entries, None);
            }
            other => panic!("expected permission respond, got {other:?}"),
        }
    }

    #[test]
    fn submission_normalizes_special_file_system_subpath_inside_workspace_roots_label() {
        let submission = Submission::decode(
            r#"{"id":"permission-response","method":"permission/respond","params":{"requestId":"permission-turn-1-request","decision":"allow","scope":"session","permissions":{"fileSystem":{"read":null,"write":null,"entries":[{"path":{"type":"special","value":{"kind":"project_roots","subpath":"/tmp/../docs"}},"access":"write"}]},"network":null}}}"#,
        )
        .expect("submission");

        match submission.op {
            ClientOp::PermissionRespond { permissions, .. } => {
                let file_system = permissions.file_system.expect("file system permissions");
                assert_eq!(
                    file_system.write,
                    Some(vec![PathBuf::from(":workspace_roots/tmp/docs")])
                );
            }
            other => panic!("expected permission respond, got {other:?}"),
        }
    }

    #[test]
    fn submission_decodes_thread_start_wire_shape() {
        let submission =
            Submission::decode(r#"{"id":"req-thread","method":"thread/start","params":{}}"#)
                .expect("submission");

        assert_eq!(submission.id, Value::from("req-thread"));
        assert_eq!(
            submission.op,
            ClientOp::ThreadStart {
                runtime_workspace_roots: None
            }
        );
    }

    #[test]
    fn submission_decodes_thread_start_runtime_workspace_roots() {
        let roots = vec![
            test_absolute_path("workspace-one"),
            test_absolute_path("workspace-two"),
        ];
        let wire = json!({
            "id": "req-thread",
            "method": "thread/start",
            "params": { "runtimeWorkspaceRoots": roots },
        });
        let submission = Submission::decode(&wire.to_string()).expect("submission");

        assert_eq!(
            submission.op,
            ClientOp::ThreadStart {
                runtime_workspace_roots: Some(roots)
            }
        );
    }

    #[test]
    fn submission_decodes_fuzzy_file_search_session_operations() {
        let roots = vec![test_absolute_path("one"), test_absolute_path("two")];
        let wire = json!({
            "id": "search-start",
            "method": "fuzzyFileSearch/sessionStart",
            "params": {
                "sessionId": "files-1",
                "roots": roots,
                "exclude": ["target/**"],
                "respectGitignore": false,
                "resultLimit": 24,
            },
        });
        let start = Submission::decode(&wire.to_string()).expect("start submission");
        assert_eq!(
            start.op,
            ClientOp::FuzzyFileSearchSessionStart {
                session_id: "files-1".to_string(),
                roots,
                exclude: vec!["target/**".to_string()],
                respect_gitignore: false,
                result_limit: 24,
            }
        );

        let update = Submission::decode(
            r#"{"id":"search-update","method":"fuzzyFileSearch/sessionUpdate","params":{"sessionId":"files-1","query":"src/mai"}}"#,
        )
        .expect("update submission");
        assert_eq!(
            update.op,
            ClientOp::FuzzyFileSearchSessionUpdate {
                session_id: "files-1".to_string(),
                query: "src/mai".to_string(),
            }
        );

        let stop = Submission::decode(
            r#"{"id":"search-stop","method":"fuzzyFileSearch/sessionStop","params":{"sessionId":"files-1"}}"#,
        )
        .expect("stop submission");
        assert_eq!(
            stop.op,
            ClientOp::FuzzyFileSearchSessionStop {
                session_id: "files-1".to_string(),
            }
        );
    }

    #[test]
    fn submission_decodes_unified_mention_search_session_operations() {
        let start = Submission::decode(
            r#"{"id":"mention-start","method":"mention/search/start","params":{"sessionId":"mentions-1","threadId":"thread-1","exclude":["target/**"],"respectGitignore":false,"resultLimit":24}}"#,
        )
        .expect("mention search start");
        assert_eq!(
            start.op,
            ClientOp::MentionSearchSessionStart {
                session_id: "mentions-1".to_string(),
                thread_id: "thread-1".to_string(),
                exclude: vec!["target/**".to_string()],
                respect_gitignore: false,
                result_limit: 24,
            }
        );

        let update = Submission::decode(
            r#"{"id":"mention-update","method":"mention/search/update","params":{"sessionId":"mentions-1","query":"review"}}"#,
        )
        .expect("mention search update");
        assert_eq!(
            update.op,
            ClientOp::MentionSearchSessionUpdate {
                session_id: "mentions-1".to_string(),
                query: "review".to_string(),
            }
        );

        let stop = Submission::decode(
            r#"{"id":"mention-stop","method":"mention/search/stop","params":{"sessionId":"mentions-1"}}"#,
        )
        .expect("mention search stop");
        assert_eq!(
            stop.op,
            ClientOp::MentionSearchSessionStop {
                session_id: "mentions-1".to_string(),
            }
        );
    }

    #[test]
    fn submission_decodes_atomic_mention_input() {
        let submission = Submission::decode(
            r#"{"id":"turn","method":"turn/start","params":{"threadId":"thread-1","input":[{"type":"text","text":"read "},{"type":"mention","name":"same.txt","target":{"type":"file","root":"/tmp/two","path":"same.txt","kind":"file"}}]}}"#,
        )
        .expect("mention submission");

        let ClientOp::SubmitWithMentions {
            thread_id,
            prompt,
            bindings,
            ..
        } = submission.op
        else {
            panic!("expected bound mention submit");
        };
        assert_eq!(thread_id.as_deref(), Some("thread-1"));
        assert_eq!(prompt, "read @same.txt");
        assert_eq!(bindings.bindings().len(), 1);
        assert_eq!(
            bindings.bindings()[0].target,
            crate::mentions::MentionTarget::File {
                root: test_protocol_path("/tmp/two"),
                path: "same.txt".to_string(),
                kind: crate::mentions::MentionFileKind::File,
            }
        );
    }

    #[test]
    fn submission_decodes_thread_resume_and_fork_wire_shapes() {
        let resume = Submission::decode(
            r#"{"id":"resume-thread","method":"thread/resume","params":{"threadId":"thread-1"}}"#,
        )
        .expect("resume submission");
        assert_eq!(
            resume.op,
            ClientOp::ThreadResume {
                thread_id: "thread-1".to_string(),
                permissions: PermissionProfileOverride::default(),
            }
        );

        let fork = Submission::decode(
            r#"{"id":"fork-thread","method":"thread/fork","params":{"threadId":"thread-1"}}"#,
        )
        .expect("fork submission");
        assert_eq!(
            fork.op,
            ClientOp::ThreadFork {
                thread_id: "thread-1".to_string(),
                permissions: PermissionProfileOverride::default(),
            }
        );
    }

    #[test]
    fn submission_decodes_thread_resume_and_fork_permission_overrides() {
        let resume = Submission::decode(
            r#"{"id":"resume-thread","method":"thread/resume","params":{"threadId":"thread-1","approvalMode":"auto-edit","permissionRules":{"rules":[{"tool":"bash","pattern":"cargo test *","decision":"prompt"}]}}}"#,
        )
        .expect("resume submission");
        match resume.op {
            ClientOp::ThreadResume {
                thread_id,
                permissions,
            } => {
                assert_eq!(thread_id, "thread-1");
                assert_eq!(
                    permissions.approval_mode,
                    Some(orca_core::approval_types::ApprovalMode::AutoEdit)
                );
                let rules = permissions.permission_rules.expect("permission rules");
                assert_eq!(rules.rules.len(), 1);
                assert_eq!(rules.rules[0].pattern, "cargo test *");
                assert_eq!(
                    rules.rules[0].decision,
                    orca_core::approval_types::Decision::Prompt
                );
            }
            other => panic!("expected thread resume, got {other:?}"),
        }

        let fork = Submission::decode(
            r#"{"id":"fork-thread","method":"thread/fork","params":{"threadId":"thread-1","approvalMode":"full-auto","permissionRules":{"rules":[]}}}"#,
        )
        .expect("fork submission");
        match fork.op {
            ClientOp::ThreadFork {
                thread_id,
                permissions,
            } => {
                assert_eq!(thread_id, "thread-1");
                assert_eq!(
                    permissions.approval_mode,
                    Some(orca_core::approval_types::ApprovalMode::FullAuto)
                );
                assert_eq!(
                    permissions
                        .permission_rules
                        .expect("permission rules")
                        .rules
                        .len(),
                    0
                );
            }
            other => panic!("expected thread fork, got {other:?}"),
        }
    }

    #[test]
    fn submission_decodes_thread_resume_and_fork_approval_policy_alias() {
        let resume = Submission::decode(
            r#"{"id":"resume-thread","method":"thread/resume","params":{"threadId":"thread-1","approvalPolicy":"never"}}"#,
        )
        .expect("resume submission");
        match resume.op {
            ClientOp::ThreadResume { permissions, .. } => {
                assert_eq!(
                    permissions.approval_mode,
                    Some(orca_core::approval_types::ApprovalMode::FullAuto)
                );
            }
            other => panic!("expected thread resume, got {other:?}"),
        }

        let fork = Submission::decode(
            r#"{"id":"fork-thread","method":"thread/fork","params":{"threadId":"thread-1","approvalPolicy":"on-request"}}"#,
        )
        .expect("fork submission");
        match fork.op {
            ClientOp::ThreadFork { permissions, .. } => {
                assert_eq!(
                    permissions.approval_mode,
                    Some(orca_core::approval_types::ApprovalMode::Suggest)
                );
            }
            other => panic!("expected thread fork, got {other:?}"),
        }
    }

    #[test]
    fn submission_decodes_thread_read_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"read-thread","method":"thread/read","params":{"threadId":"thread-1","includeMessages":true,"includeTurns":true}}"#,
        )
        .expect("submission");

        assert_eq!(submission.id, Value::from("read-thread"));
        assert_eq!(
            submission.op,
            ClientOp::ThreadRead {
                thread_id: "thread-1".to_string(),
                include_messages: true,
                include_turns: true
            }
        );
    }

    #[test]
    fn submission_decodes_thread_metadata_update_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"rename-thread","method":"thread/metadata/update","params":{"threadId":"thread-1","title":"renamed thread"}}"#,
        )
        .expect("submission");

        assert_eq!(submission.id, Value::from("rename-thread"));
        assert_eq!(
            submission.op,
            ClientOp::ThreadMetadataUpdate {
                thread_id: "thread-1".to_string(),
                title: Some("renamed thread".to_string())
            }
        );
    }

    #[test]
    fn submission_decodes_prompt_queue_wire_contract_bits_spec_ut() {
        let queue_id = uuid::Uuid::now_v7().to_string();
        let add = Submission::decode(
            r#"{"id":"queue-add","method":"thread/queue/add","params":{"threadId":"thread-1","input":"follow up"}}"#,
        )
        .expect("queue add submission");
        assert!(matches!(
            add.op,
            ClientOp::ThreadQueue {
                thread_id,
                action: crate::prompt_queue::PromptQueueAction::Add { input },
            } if thread_id == "thread-1" && input.text == "follow up"
        ));

        let update = Submission::decode(&format!(
            r#"{{"id":"queue-update","method":"thread/queue/update","params":{{"threadId":"thread-1","expectedRevision":7,"queueId":"{queue_id}","input":"edited"}}}}"#
        ))
        .expect("queue update submission");
        assert!(matches!(
            update.op,
            ClientOp::ThreadQueue {
                action: crate::prompt_queue::PromptQueueAction::Update {
                    expected_revision,
                    input,
                    ..
                },
                ..
            } if expected_revision.get() == 7 && input.text == "edited"
        ));

        let missing_revision = Submission::decode(&format!(
            r#"{{"id":"queue-update-missing-revision","method":"thread/queue/update","params":{{"threadId":"thread-1","queueId":"{queue_id}","input":"edited"}}}}"#
        ))
        .expect_err("queue mutation without expectedRevision must fail");
        assert_eq!(
            missing_revision.id,
            Value::from("queue-update-missing-revision")
        );
        assert!(missing_revision.message.contains("expectedRevision"));
    }

    #[test]
    fn submission_decodes_thread_list_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"list-threads","method":"thread/list","params":{"cursor":"1","limit":2,"sortKey":"createdAt","sortDirection":"asc","searchTerm":"needle","archived":true}}"#,
        )
        .expect("submission");

        assert_eq!(submission.id, Value::from("list-threads"));
        assert_eq!(
            submission.op,
            ClientOp::ThreadList {
                cursor: Some("1".to_string()),
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Asc,
                search_term: Some("needle".to_string()),
                limit: 2,
                filters: ThreadListFilters {
                    archived: true,
                    ..ThreadListFilters::default()
                }
            }
        );
    }

    #[test]
    fn submission_decodes_thread_list_filter_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"list-threads","method":"thread/list","params":{"cwd":["/tmp/a","/tmp/b"],"modelProviders":["deepseek","openai"],"model":"deepseek-v4-flash","parentThreadId":"parent-1","archived":false}}"#,
        )
        .expect("submission");

        assert_eq!(
            submission.op,
            ClientOp::ThreadList {
                cursor: None,
                sort_key: ThreadSortKey::UpdatedAt,
                sort_direction: SortDirection::Desc,
                search_term: None,
                limit: 50,
                filters: ThreadListFilters {
                    archived: false,
                    model_providers: Some(vec!["deepseek".to_string(), "openai".to_string()]),
                    model_names: Some(vec!["deepseek-v4-flash".to_string()]),
                    cwd_filters: vec!["/tmp/a".to_string(), "/tmp/b".to_string()],
                    relation: Some(ThreadRelationFilter::DirectChildrenOf(
                        "parent-1".to_string()
                    )),
                }
            }
        );
    }

    #[test]
    fn submission_decodes_thread_list_ancestor_filter_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"list-threads","method":"thread/list","params":{"cwd":"/tmp/root","ancestorThreadId":"ancestor-1"}}"#,
        )
        .expect("submission");

        assert_eq!(
            submission.op,
            ClientOp::ThreadList {
                cursor: None,
                sort_key: ThreadSortKey::UpdatedAt,
                sort_direction: SortDirection::Desc,
                search_term: None,
                limit: 50,
                filters: ThreadListFilters {
                    cwd_filters: vec!["/tmp/root".to_string()],
                    relation: Some(ThreadRelationFilter::DescendantsOf(
                        "ancestor-1".to_string()
                    )),
                    ..ThreadListFilters::default()
                }
            }
        );
    }

    #[test]
    fn submission_decodes_thread_search_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"search-threads","method":"thread/search","params":{"searchTerm":"needle","cursor":"1","limit":3,"sortKey":"updatedAt","sortDirection":"desc","archived":false}}"#,
        )
        .expect("submission");

        assert_eq!(submission.id, Value::from("search-threads"));
        assert_eq!(
            submission.op,
            ClientOp::ThreadSearch {
                query: "needle".to_string(),
                cursor: Some("1".to_string()),
                sort_key: ThreadSortKey::UpdatedAt,
                sort_direction: SortDirection::Desc,
                include_archived: false,
                limit: 3
            }
        );
    }

    #[test]
    fn submission_decodes_thread_turns_list_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"list-turns","method":"thread/turns/list","params":{"threadId":"thread-1","cursor":"1","limit":2,"sortDirection":"desc"}}"#,
        )
        .expect("submission");

        assert_eq!(submission.id, Value::from("list-turns"));
        assert_eq!(
            submission.op,
            ClientOp::ThreadTurnsList {
                thread_id: "thread-1".to_string(),
                cursor: Some("1".to_string()),
                sort_direction: SortDirection::Desc,
                items_view: TurnItemsView::Full,
                limit: 2
            }
        );
    }

    #[test]
    fn submission_decodes_thread_turns_list_items_view_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"list-turns","method":"thread/turns/list","params":{"threadId":"thread-1","itemsView":"notLoaded"}}"#,
        )
        .expect("submission");

        assert_eq!(
            submission.op,
            ClientOp::ThreadTurnsList {
                thread_id: "thread-1".to_string(),
                cursor: None,
                sort_direction: SortDirection::Asc,
                items_view: TurnItemsView::NotLoaded,
                limit: 50
            }
        );
    }

    #[test]
    fn submission_decodes_thread_items_list_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"list-items","method":"thread/items/list","params":{"threadId":"thread-1","turnId":"turn-1","cursor":"2","limit":5,"sortDirection":"asc"}}"#,
        )
        .expect("submission");

        assert_eq!(submission.id, Value::from("list-items"));
        assert_eq!(
            submission.op,
            ClientOp::ThreadItemsList {
                thread_id: "thread-1".to_string(),
                turn_id: Some("turn-1".to_string()),
                cursor: Some("2".to_string()),
                sort_direction: SortDirection::Asc,
                limit: 5
            }
        );
    }

    #[test]
    fn submission_decodes_turn_start_text_inputs_in_order() {
        let submission = Submission::decode(
            r#"{"id":"req-1","method":"turn/start","params":{"input":[{"type":"text","text":"first"},{"type":"image","url":"https://example.test/image.png"},{"type":"text","text":"second"}]}}"#,
        )
        .expect("submission");

        assert_eq!(
            submission.op,
            ClientOp::Submit {
                thread_id: None,
                prompt: "first\nsecond".to_string(),
                permissions: PermissionProfileOverride::default()
            }
        );
    }

    #[test]
    fn submission_decodes_turn_control_wire_shapes() {
        let interrupt = Submission::decode(
            r#"{"id":"interrupt","method":"turn/interrupt","params":{"turnId":"turn-1"}}"#,
        )
        .expect("interrupt submission");
        assert_eq!(
            interrupt.op,
            ClientOp::TurnInterrupt {
                thread_id: None,
                turn_id: "turn-1".to_string()
            }
        );

        let resume = Submission::decode(
            r#"{"id":"resume","method":"turn/resume","params":{"threadId":"thread-1","turnId":"turn-1"}}"#,
        )
        .expect("resume submission");
        assert_eq!(
            resume.op,
            ClientOp::TurnResume {
                thread_id: Some("thread-1".to_string()),
                turn_id: "turn-1".to_string()
            }
        );

        let steer = Submission::decode(
            r#"{"id":"steer","method":"turn/steer","params":{"turnId":"turn-1","input":[{"type":"text","text":"first"},{"type":"text","text":"second"}]}}"#,
        )
        .expect("steer submission");
        assert_eq!(
            steer.op,
            ClientOp::TurnSteer {
                thread_id: None,
                turn_id: "turn-1".to_string(),
                input: "first\nsecond".to_string(),
            }
        );
    }

    #[test]
    fn submission_decodes_permission_response_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"perm-response","method":"permission/respond","params":{"requestId":"perm-1","decision":"allow","scope":"turn","permissions":{"fileSystem":{"write":["/tmp/extra"],"read":null},"network":null}}}"#,
        )
        .expect("permission/respond submission");

        assert_eq!(submission.id, Value::from("perm-response"));
        assert_eq!(
            submission.op,
            ClientOp::PermissionRespond {
                request_id: "perm-1".to_string(),
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![test_protocol_path("/tmp/extra")]),
                        entries: None,
                    }),
                    network: None,
                    shell: None,
                },
                strict_auto_review: false,
            }
        );
    }

    #[test]
    fn submission_rejects_permission_response_without_request_id() {
        let error = Submission::decode(
            r#"{"id":"perm-response","method":"permission/respond","params":{"decision":"allow","scope":"turn","permissions":{"fileSystem":{"write":["/tmp/extra"],"read":null},"network":null}}}"#,
        )
        .expect_err("permission/respond must include requestId");

        assert_eq!(error.id, Value::from("perm-response"));
        assert_eq!(
            error.message,
            "permission/respond params.requestId is required"
        );
    }

    #[test]
    fn submission_decodes_user_input_response_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"input-response","method":"user_input/respond","params":{"requestId":"input-turn-1-ask","answer":"ship it"}}"#,
        )
        .expect("user_input/respond submission");

        assert_eq!(submission.id, Value::from("input-response"));
        assert_eq!(
            submission.op,
            ClientOp::UserInputRespond {
                request_id: "input-turn-1-ask".to_string(),
                answer: Some("ship it".to_string()),
            }
        );
    }

    #[test]
    fn submission_decodes_mcp_elicitation_response_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"mcp-response","method":"mcp_elicitation/respond","params":{"requestId":"mcp_elicitation:github:device-flow","accepted":true,"contentJson":{"code":"ABCD-1234"}}}"#,
        )
        .expect("mcp_elicitation/respond submission");

        assert_eq!(submission.id, Value::from("mcp-response"));
        assert_eq!(
            submission.op,
            ClientOp::McpElicitationRespond {
                request_id: "mcp_elicitation:github:device-flow".to_string(),
                accepted: true,
                content_json: Some(json!({"code": "ABCD-1234"})),
            }
        );
    }

    #[test]
    fn submission_rejects_mcp_elicitation_response_without_request_id() {
        let error = Submission::decode(
            r#"{"id":"mcp-response","method":"mcp_elicitation/respond","params":{"accepted":false}}"#,
        )
        .expect_err("mcp_elicitation/respond must include requestId");

        assert_eq!(error.id, Value::from("mcp-response"));
        assert_eq!(
            error.message,
            "mcp_elicitation/respond params.requestId is required"
        );
    }

    #[test]
    fn submission_rejects_user_input_response_without_request_id() {
        let error = Submission::decode(
            r#"{"id":"input-response","method":"user_input/respond","params":{"answer":"ship it"}}"#,
        )
        .expect_err("user_input/respond must include requestId");

        assert_eq!(error.id, Value::from("input-response"));
        assert_eq!(
            error.message,
            "user_input/respond params.requestId is required"
        );
    }

    #[test]
    fn submission_decodes_permission_response_file_system_entries() {
        let submission = Submission::decode(
            r#"{"id":"perm-response","method":"permission/respond","params":{"requestId":"perm-1","decision":"allow","scope":"session","permissions":{"fileSystem":{"read":null,"write":null,"entries":[{"path":"/tmp/readable","access":"read"},{"path":"/tmp/writable","access":"write"},{"path":"/tmp/both","access":"readWrite"}]},"network":null}}}"#,
        )
        .expect("permission/respond submission");

        assert_eq!(
            submission.op,
            ClientOp::PermissionRespond {
                request_id: "perm-1".to_string(),
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Session,
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: Some(vec![
                            test_protocol_path("/tmp/readable"),
                            test_protocol_path("/tmp/both"),
                        ]),
                        write: Some(vec![
                            test_protocol_path("/tmp/writable"),
                            test_protocol_path("/tmp/both"),
                        ]),
                        entries: None,
                    }),
                    network: None,
                    shell: None,
                },
                strict_auto_review: false,
            }
        );
    }

    #[test]
    fn submission_decodes_permission_response_strict_auto_review() {
        let submission = Submission::decode(
            r#"{"id":"perm-response","method":"permission/respond","params":{"requestId":"perm-1","decision":"allow","scope":"turn","strictAutoReview":true,"permissions":{"fileSystem":{"write":["/tmp/extra"],"read":null},"network":null}}}"#,
        )
        .expect("permission/respond submission");

        assert_eq!(
            submission.op,
            ClientOp::PermissionRespond {
                request_id: "perm-1".to_string(),
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![test_protocol_path("/tmp/extra")]),
                        entries: None,
                    }),
                    network: None,
                    shell: None,
                },
                strict_auto_review: true,
            }
        );
    }

    #[test]
    fn submission_decodes_permission_response_shell_unsandboxed() {
        let submission = Submission::decode(
            r#"{"id":"perm-response","method":"permission/respond","params":{"requestId":"perm-1","decision":"allow","scope":"turn","permissions":{"shell":{"unsandboxed":true}}}}"#,
        )
        .expect("permission/respond submission");

        let ClientOp::PermissionRespond { permissions, .. } = submission.op else {
            panic!("expected permission response");
        };

        assert!(permissions.file_system.is_none());
        assert!(permissions.network.is_none());
        assert!(
            permissions
                .shell
                .as_ref()
                .is_some_and(|shell| shell.unsandboxed)
        );
    }

    #[test]
    fn submission_decodes_permission_response_network_domain_grants() {
        let submission = Submission::decode(
            r#"{"id":"perm-response","method":"permission/respond","params":{"requestId":"perm-1","decision":"allow","scope":"session","permissions":{"fileSystem":null,"network":{"enabled":true,"domains":{"api.example.com":"allow","blocked.example.com":"deny"}}}}}"#,
        )
        .expect("permission/respond submission");

        let ClientOp::PermissionRespond {
            permissions, scope, ..
        } = submission.op
        else {
            panic!("expected permission response op");
        };
        assert_eq!(scope, PermissionGrantScope::Session);
        let network = permissions.network.expect("network permission grants");
        assert_eq!(network.enabled, Some(true));
        assert_eq!(
            network.domains.get("api.example.com"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Allow)
        );
        assert_eq!(
            network.domains.get("blocked.example.com"),
            Some(&orca_core::config::PermissionProfileNetworkAccess::Deny)
        );
    }

    #[test]
    fn submission_decodes_shell_resize_wire_shape() {
        let resize = Submission::decode(
            r#"{"id":"resize","method":"shell/resize","params":{"shellId":"shell-1","cols":120,"rows":33}}"#,
        )
        .expect("resize submission");

        assert_eq!(resize.id, Value::from("resize"));
        assert_eq!(
            resize.op,
            ClientOp::ShellResize {
                shell_id: "shell-1".to_string(),
                cols: 120,
                rows: 33,
            }
        );
    }

    #[test]
    fn submission_decodes_shell_list_wire_shape() {
        let list = Submission::decode(r#"{"id":"list","method":"shell/list","params":{}}"#)
            .expect("shell/list submission");

        assert_eq!(list.id, Value::from("list"));
        assert_eq!(list.op, ClientOp::ShellList);
    }

    #[test]
    fn submission_decodes_shell_read_output_cap_wire_shape() {
        let read = Submission::decode(
            r#"{"id":"read","method":"shell/read","params":{"shellId":"shell-1","timeoutMs":5000,"outputBytesCap":256}}"#,
        )
        .expect("shell/read submission");

        assert_eq!(read.id, Value::from("read"));
        assert_eq!(
            read.op,
            ClientOp::ShellRead {
                shell_id: "shell-1".to_string(),
                timeout_ms: 5000,
                output_bytes_cap: Some(256),
            }
        );
    }

    #[test]
    fn submission_decodes_shell_update_wire_shape() {
        let update = Submission::decode(
            r#"{"id":"update","method":"shell/update","params":{"shellId":"shell-1","description":"renamed shell"}}"#,
        )
        .expect("shell/update submission");

        assert_eq!(update.id, Value::from("update"));
        assert_eq!(
            update.op,
            ClientOp::ShellUpdate {
                shell_id: "shell-1".to_string(),
                description: Some("renamed shell".to_string())
            }
        );
    }

    #[test]
    fn submission_decodes_turn_start_thread_id() {
        let submission = Submission::decode(
            r#"{"id":"req-1","method":"turn/start","params":{"threadId":"thread-1","input":[{"type":"text","text":"hello"}]}}"#,
        )
        .expect("submission");

        assert_eq!(
            submission.op,
            ClientOp::Submit {
                thread_id: Some("thread-1".to_string()),
                prompt: "hello".to_string(),
                permissions: PermissionProfileOverride::default()
            }
        );
    }

    #[test]
    fn submission_decodes_turn_start_runtime_workspace_roots() {
        let root = test_absolute_path("new-root");
        let wire = json!({
            "id": "req-1",
            "method": "turn/start",
            "params": {
                "threadId": "thread-1",
                "runtimeWorkspaceRoots": [root],
                "input": [{ "type": "text", "text": "hello" }],
            },
        });
        let submission = Submission::decode(&wire.to_string()).expect("submission");

        match submission.op {
            ClientOp::Submit { permissions, .. } => {
                assert_eq!(permissions.runtime_workspace_roots, Some(vec![root]))
            }
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn submission_keeps_request_id_for_unsupported_ops() {
        let error = Submission::decode(r#"{"id":"req-1","op":"interrupt"}"#).expect_err("error");

        assert_eq!(error.id, Value::from("req-1"));
        assert_eq!(error.message, "unsupported op: interrupt");
    }

    #[test]
    fn submission_decodes_shell_start_terminal_mode_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"shell","method":"shell/start","params":{"threadId":"thread-1","command":"bash","description":"terminal","terminalMode":"pty","cols":132,"rows":41}}"#,
        )
        .expect("shell/start submission");

        assert_eq!(
            submission.op,
            ClientOp::ShellStart {
                thread_id: Some("thread-1".to_string()),
                command: "bash".to_string(),
                description: Some("terminal".to_string()),
                terminal: crate::shell_session::ShellTerminalMode::pty(Some(132), Some(41))
            }
        );

        let legacy = Submission::decode(
            r#"{"id":"shell","method":"shell/start","params":{"command":"bash","pty":true}}"#,
        )
        .expect("legacy shell/start submission");
        assert_eq!(
            legacy.op,
            ClientOp::ShellStart {
                thread_id: None,
                command: "bash".to_string(),
                description: None,
                terminal: crate::shell_session::ShellTerminalMode::pty(None, None)
            }
        );
    }

    #[test]
    fn submission_decodes_command_exec_wire_shape() {
        let submission = Submission::decode(
            // windows-platform-boundary: protocol-shape-only
            r#"{"id":"cmd","method":"command/exec","params":{"threadId":"thread-1","command":["sh","-lc","printf ok"],"processId":"process-1","cwd":"/tmp/orca-command","env":{"ORCA_COMMAND_EXEC_BASE":"request","ORCA_COMMAND_EXEC_REMOVE":null},"tty":false,"streamStdin":true,"streamStdoutStderr":true,"outputBytesCap":1024,"disableTimeout":false,"timeoutMs":5000,"permissionProfile":"read-only"}}"#,
        )
        .expect("command/exec submission");

        assert_eq!(
            submission.op,
            ClientOp::CommandExec {
                thread_id: Some("thread-1".to_string()),
                // windows-platform-boundary: protocol-shape-only
                command: vec!["sh".to_string(), "-lc".to_string(), "printf ok".to_string()],
                command_is_argv: true,
                process_id: Some("process-1".to_string()),
                cwd: Some(test_protocol_path("/tmp/orca-command")),
                env: BTreeMap::from([
                    (
                        "ORCA_COMMAND_EXEC_BASE".to_string(),
                        Some("request".to_string())
                    ),
                    ("ORCA_COMMAND_EXEC_REMOVE".to_string(), None),
                ]),
                options: CommandExecOptions {
                    stream_stdin: true,
                    stream_stdout_stderr: true,
                    has_size: false,
                    output_bytes_cap: Some(1024),
                    disable_output_cap: false,
                    disable_timeout: false,
                    timeout_ms: Some(5000),
                    sandbox_policy: CommandSandboxPolicy::Default,
                    permission_profile: Some("read-only".to_string()),
                },
                terminal: crate::shell_session::ShellTerminalMode::pipe(),
            }
        );
    }

    #[test]
    fn submission_preserves_legacy_command_exec_script_shape() {
        let submission = Submission::decode(
            r#"{"id":"cmd","method":"command/exec","params":{"command":"printf ok"}}"#,
        )
        .expect("legacy command/exec submission");

        match submission.op {
            ClientOp::CommandExec {
                command,
                command_is_argv,
                ..
            } => {
                assert_eq!(command, ["printf ok"]);
                assert!(!command_is_argv);
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn submission_decodes_command_exec_list_wire_shape() {
        let list =
            Submission::decode(r#"{"id":"cmd-list","method":"command/exec/list","params":{}}"#)
                .expect("command/exec/list submission");

        assert_eq!(list.id, Value::from("cmd-list"));
        assert_eq!(list.op, ClientOp::CommandExecList);
    }

    #[test]
    fn submission_decodes_command_exec_tty_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"cmd","method":"command/exec","params":{"command":["sh"],"tty":true,"size":{"cols":100,"rows":40}}}"#,
        )
        .expect("command/exec tty submission");

        assert_eq!(
            submission.op,
            ClientOp::CommandExec {
                thread_id: None,
                command: vec!["sh".to_string()],
                command_is_argv: true,
                process_id: None,
                cwd: None,
                env: BTreeMap::new(),
                options: CommandExecOptions {
                    has_size: true,
                    ..CommandExecOptions::default()
                },
                terminal: crate::shell_session::ShellTerminalMode::pty(Some(100), Some(40)),
            }
        );
    }

    #[test]
    fn submission_decodes_command_exec_workspace_write_sandbox_policy() {
        let submission = Submission::decode(
            // windows-platform-boundary: protocol-shape-only
            r#"{"id":"cmd","method":"command/exec","params":{"command":["sh","-lc","true"],"sandboxPolicy":{"type":"workspaceWrite","writableRoots":["/tmp/allowed"],"networkAccess":true,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}}}"#,
        )
        .expect("command/exec workspaceWrite submission");

        match submission.op {
            ClientOp::CommandExec { options, .. } => {
                assert_eq!(
                    options.sandbox_policy,
                    CommandSandboxPolicy::WorkspaceWrite {
                        writable_roots: vec![test_protocol_path("/tmp/allowed")],
                        network_access: true,
                        exclude_tmpdir_env_var: false,
                        exclude_slash_tmp: false,
                    }
                );
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn submission_decodes_command_exec_read_only_sandbox_policy() {
        let submission = Submission::decode(
            // windows-platform-boundary: protocol-shape-only
            r#"{"id":"cmd","method":"command/exec","params":{"command":["sh","-lc","true"],"sandboxPolicy":{"type":"readOnly","networkAccess":true}}}"#,
        )
        .expect("command/exec readOnly submission");

        match submission.op {
            ClientOp::CommandExec { options, .. } => {
                assert_eq!(
                    options.sandbox_policy,
                    CommandSandboxPolicy::ReadOnly {
                        network_access: true
                    }
                );
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn submission_decodes_command_exec_external_sandbox_policy() {
        let submission = Submission::decode(
            // windows-platform-boundary: protocol-shape-only
            r#"{"id":"cmd","method":"command/exec","params":{"command":["sh","-lc","true"],"sandboxPolicy":{"type":"externalSandbox","networkAccess":"enabled"}}}"#,
        )
        .expect("command/exec externalSandbox submission");

        match submission.op {
            ClientOp::CommandExec { options, .. } => {
                assert_eq!(
                    options.sandbox_policy,
                    CommandSandboxPolicy::ExternalSandbox {
                        network_access: NetworkAccess::Enabled
                    }
                );
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn submission_decodes_command_exec_write_and_resize_wire_shapes() {
        let write = Submission::decode(
            r#"{"id":"cmd-write","method":"command/exec/write","params":{"processId":"process-1","deltaBase64":"aGVsbG8K","closeStdin":true}}"#,
        )
        .expect("command/exec/write submission");
        assert_eq!(
            write.op,
            ClientOp::CommandExecWrite {
                process_id: "process-1".to_string(),
                delta_base64: Some("aGVsbG8K".to_string()),
                close_stdin: true,
            }
        );

        let read = Submission::decode(
            r#"{"id":"cmd-read","method":"command/exec/read","params":{"processId":"process-1","timeoutMs":5000,"outputBytesCap":1024}}"#,
        )
        .expect("command/exec/read submission");
        assert_eq!(
            read.op,
            ClientOp::CommandExecRead {
                process_id: "process-1".to_string(),
                timeout_ms: 5000,
                output_bytes_cap: Some(1024),
            }
        );

        let resize = Submission::decode(
            r#"{"id":"cmd-resize","method":"command/exec/resize","params":{"processId":"process-1","size":{"cols":120,"rows":33}}}"#,
        )
        .expect("command/exec/resize submission");
        assert_eq!(
            resize.op,
            ClientOp::CommandExecResize {
                process_id: "process-1".to_string(),
                cols: 120,
                rows: 33,
            }
        );
    }

    #[test]
    fn submission_decodes_command_exec_terminate_wire_shape() {
        let submission = Submission::decode(
            r#"{"id":"cmd-kill","method":"command/exec/terminate","params":{"processId":"process-1"}}"#,
        )
        .expect("command/exec/terminate submission");

        assert_eq!(
            submission.op,
            ClientOp::CommandExecTerminate {
                process_id: "process-1".to_string()
            }
        );
    }

    #[test]
    fn server_event_serializes_legacy_flat_shape() {
        let value = legacy_json_event(
            Value::from(7),
            ServerEvent::ToolCompleted {
                tool_call_id: Value::Null,
                tool: Value::from("read_file"),
                status: Value::from("completed"),
                output: Value::from("ok"),
                error: Value::Null,
                exit_code: Value::Null,
                kind: Value::Null,
                terminal_source: Value::Null,
            },
        );

        assert_eq!(value["id"], 7);
        assert_eq!(value["event"], "tool_completed");
        assert_eq!(value["tool"], "read_file");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["output"], "ok");
        assert!(value.get("error").is_none());
        assert!(value.get("type").is_none());
    }

    #[test]
    fn server_event_serializes_shell_streaming_notifications() {
        let delta = legacy_json_event(
            Value::from("shell-read"),
            ServerEvent::ShellOutputDelta {
                shell_id: Value::from("shell-1"),
                stream: Value::from("stdout"),
                delta: Value::from("ready"),
                cap_reached: Value::from(false),
                final_chunk: Value::from(false),
            },
        );

        assert_eq!(delta["event"], "shell_output_delta");
        assert_eq!(delta["shellId"], "shell-1");
        assert_eq!(delta["stream"], "stdout");
        assert_eq!(delta["delta"], "ready");
        assert_eq!(delta["capReached"], false);
        assert_eq!(delta["final"], false);

        let exited = legacy_json_event(
            Value::from("shell-read"),
            ServerEvent::ShellExited {
                shell_id: Value::from("shell-1"),
                task_id: Value::from("task-1"),
                status: Value::from("completed"),
                exit_code: Value::from(0),
            },
        );

        assert_eq!(exited["event"], "shell_exited");
        assert_eq!(exited["shellId"], "shell-1");
        assert_eq!(exited["taskId"], "task-1");
        assert_eq!(exited["status"], "completed");
        assert_eq!(exited["exitCode"], 0);
    }

    #[test]
    fn server_event_serializes_command_exec_output_delta_as_jsonrpc_notification() {
        let delta = legacy_json_event(
            Value::Null,
            ServerEvent::CommandExecOutputDelta {
                process_id: Value::from("process-1"),
                stream: Value::from("stdout"),
                delta: Value::from("ready"),
                delta_base64: Value::from("cmVhZHk="),
                cap_reached: Value::from(false),
                final_chunk: Value::from(false),
            },
        );

        assert_eq!(delta["event"], "command_exec_output_delta");
        assert_eq!(delta["processId"], "process-1");
        assert_eq!(delta["method"], "command/exec/outputDelta");
        assert_eq!(delta["params"]["processId"], "process-1");
        assert_eq!(delta["params"]["stream"], "stdout");
        assert_eq!(delta["params"]["deltaBase64"], "cmVhZHk=");
        assert_eq!(delta["params"]["capReached"], false);
        assert!(delta["params"].get("delta").is_none());
        assert!(delta["params"].get("final").is_none());
    }

    #[test]
    fn runtime_tool_completed_mapping_preserves_failure_error() {
        let mapped = map_runtime_event_line(
            r#"{"type":"tool.call.completed","payload":{"id":"tool-1","name":"mcp__slow__wait","status":"failed","error":"MCP request 'tools/call' timed out after 100ms","exit_code":124,"kind":"runtime_error"}}"#,
        )
        .expect("mapped event");
        let value = legacy_json_event(Value::from("turn-1"), mapped);

        assert_eq!(value["event"], "tool_completed");
        assert_eq!(value["tool"], "mcp__slow__wait");
        assert_eq!(value["status"], "failed");
        assert_eq!(
            value["error"],
            "MCP request 'tools/call' timed out after 100ms"
        );
        assert_eq!(value["exitCode"], 124);
        assert_eq!(value["kind"], "runtime_error");
        assert!(value.get("output").is_none());
    }

    #[test]
    fn runtime_tool_completed_mapping_preserves_terminal_identity_and_source() {
        let mapped = map_runtime_event_line(
            r#"{"type":"tool.call.completed","payload":{"id":"legacy-call","name":"bash","status":"indeterminate","error":"missing terminal result","kind":"indeterminate","terminal_source":"compatibility_repair"}}"#,
        )
        .expect("mapped event");
        let value = legacy_json_event(Value::from("turn-1"), mapped);

        assert_eq!(value["event"], "tool_completed");
        assert_eq!(value["toolCallId"], "legacy-call");
        assert_eq!(value["status"], "indeterminate");
        assert_eq!(value["kind"], "indeterminate");
        assert_eq!(value["terminalSource"], "compatibility_repair");
    }

    #[test]
    fn server_event_serializes_item_started_user_message() {
        let value = legacy_json_event(
            Value::from("steer-1"),
            ServerEvent::ItemStarted {
                thread_id: Value::from("thread-1"),
                turn_id: Value::from("thread-1:task-1"),
                item: json!({
                    "type": "user_message",
                    "role": "user",
                    "content": "steer",
                }),
            },
        );

        assert_eq!(value["id"], "steer-1");
        assert_eq!(value["event"], "item_started");
        assert_eq!(value["threadId"], "thread-1");
        assert_eq!(value["turnId"], "thread-1:task-1");
        assert_eq!(value["item"]["type"], "user_message");
        assert_eq!(value["item"]["role"], "user");
        assert_eq!(value["item"]["content"], "steer");
        assert!(value.get("type").is_none());
    }

    #[test]
    fn server_event_serializes_agent_message_item_lifecycle() {
        let delta = legacy_json_event(
            Value::from("turn-1"),
            ServerEvent::ItemMessageDelta {
                item_id: Value::from("item-agent-message-1"),
                delta: Value::from("hello"),
            },
        );
        assert_eq!(delta["event"], "item_message_delta");
        assert_eq!(delta["itemId"], "item-agent-message-1");
        assert_eq!(delta["delta"], "hello");

        let completed = legacy_json_event(
            Value::from("turn-1"),
            ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": "item-agent-message-1",
                    "type": "agent_message",
                    "text": "hello",
                }),
            },
        );
        assert_eq!(completed["event"], "item_completed");
        assert_eq!(completed["item"]["id"], "item-agent-message-1");
        assert_eq!(completed["item"]["type"], "agent_message");
        assert_eq!(completed["item"]["text"], "hello");
    }

    #[test]
    fn server_event_serializes_plan_item_lifecycle() {
        let delta = legacy_json_event(
            Value::from("turn-1"),
            ServerEvent::ItemPlanDelta {
                item_id: Value::from("item-plan-1"),
                delta: Value::from("# Plan\n"),
            },
        );
        assert_eq!(delta["event"], "item_plan_delta");
        assert_eq!(delta["itemId"], "item-plan-1");
        assert_eq!(delta["delta"], "# Plan\n");

        let completed = legacy_json_event(
            Value::from("turn-1"),
            ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": "item-plan-1",
                    "type": "plan",
                    "text": "# Plan\n",
                }),
            },
        );
        assert_eq!(completed["event"], "item_completed");
        assert_eq!(completed["item"]["id"], "item-plan-1");
        assert_eq!(completed["item"]["type"], "plan");
        assert_eq!(completed["item"]["text"], "# Plan\n");
    }

    #[test]
    fn server_event_serializes_reasoning_item_lifecycle() {
        let delta = legacy_json_event(
            Value::from("turn-1"),
            ServerEvent::ItemReasoningDelta {
                item_id: Value::from("item-reasoning-1"),
                delta: Value::from("thinking"),
            },
        );
        assert_eq!(delta["event"], "item_reasoning_delta");
        assert_eq!(delta["itemId"], "item-reasoning-1");
        assert_eq!(delta["delta"], "thinking");

        let completed = legacy_json_event(
            Value::from("turn-1"),
            ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": "item-reasoning-1",
                    "type": "reasoning",
                    "summary": "thinking",
                    "content": "",
                }),
            },
        );
        assert_eq!(completed["event"], "item_completed");
        assert_eq!(completed["item"]["id"], "item-reasoning-1");
        assert_eq!(completed["item"]["type"], "reasoning");
        assert_eq!(completed["item"]["summary"], "thinking");
        assert_eq!(completed["item"]["content"], "");
    }

    #[test]
    fn server_event_serializes_command_execution_item_completion() {
        let completed = legacy_json_event(
            Value::from("turn-1"),
            ServerEvent::ItemCompleted {
                thread_id: Value::Null,
                turn_id: Value::Null,
                item: json!({
                    "id": "tool-1",
                    "type": "commandExecution",
                    "tool": "bash",
                    "command": "cargo test",
                    "status": "completed",
                    "aggregatedOutput": "ok",
                    "exitCode": 0,
                }),
            },
        );

        assert_eq!(completed["event"], "item_completed");
        assert_eq!(completed["item"]["id"], "tool-1");
        assert_eq!(completed["item"]["type"], "commandExecution");
        assert_eq!(completed["item"]["tool"], "bash");
        assert_eq!(completed["item"]["command"], "cargo test");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["aggregatedOutput"], "ok");
        assert!(completed["item"].get("output").is_none());
        assert_eq!(completed["item"]["exitCode"], 0);
    }

    #[test]
    fn maps_runtime_turn_started_task_lifecycle_metadata() {
        let event = map_runtime_event_line(
            r#"{"type":"turn.started","payload":{"turn":1,"task":{"task_id":"run-1:task-1","kind":"agent","status":"running","turn":1}}}"#,
        )
        .expect("event");
        let value = legacy_json_event(Value::from(7), event);

        assert_eq!(value["event"], "turn_started");
        assert_eq!(value["turn"], 1);
        assert_eq!(value["task"]["task_id"], "run-1:task-1");
        assert_eq!(value["task"]["kind"], "agent");
        assert_eq!(value["task"]["status"], "running");
    }

    #[test]
    fn maps_runtime_workflow_task_lifecycle_metadata() {
        let event = map_runtime_event_line(
            r#"{"type":"workflow.started","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit","task":{"task_id":"workflow-run-1:task-1","kind":"workflow","status":"running","turn":0}}}"#,
        )
        .expect("event");
        let value = legacy_json_event(Value::from(7), event);

        assert_eq!(value["event"], "workflow_started");
        assert_eq!(value["task"]["task_id"], "workflow-run-1:task-1");
        assert_eq!(value["task"]["kind"], "workflow");
        assert_eq!(value["task"]["status"], "running");
    }

    #[test]
    fn maps_runtime_task_status_update_to_protocol_shape() {
        let event = map_runtime_event_line(
            r#"{"type":"task.status.updated","payload":{"task":{"id":"main-session-1","type":"main_session","status":"approval_required","isBackgrounded":true,"description":"background turn","createdAtMs":10,"startedAtMs":20,"tool":"shell"}}}"#,
        )
        .expect("task status event");
        let value = legacy_json_event(Value::from("task-update-1"), event);

        assert_eq!(value["id"], "task-update-1");
        assert_eq!(value["event"], "task_status_updated");
        assert_eq!(value["task"]["id"], "main-session-1");
        assert_eq!(value["task"]["type"], "main_session");
        assert_eq!(value["task"]["status"], "approval_required");
        assert_eq!(value["task"]["isBackgrounded"], true);
        assert_eq!(value["task"]["tool"], "shell");
    }

    #[test]
    fn maps_runtime_workflow_lifecycle_events_to_protocol_shape() {
        let cases = [
            (
                r#"{"type":"workflow.phase.started","payload":{"taskId":"task-1","runId":"workflow-run-1","phase":"scan"}}"#,
                "workflow_phase_started",
                "scan",
            ),
            (
                r#"{"type":"workflow.agent.started","payload":{"taskId":"task-1","runId":"workflow-run-1","phase":"scan","agentId":"agent-1"}}"#,
                "workflow_agent_started",
                "agent-1",
            ),
            (
                r#"{"type":"workflow.agent.failed","payload":{"taskId":"task-1","runId":"workflow-run-1","phase":"scan","agentId":"agent-1","error":"boom"}}"#,
                "workflow_agent_failed",
                "boom",
            ),
            (
                r#"{"type":"workflow.paused","payload":{"taskId":"task-1","runId":"workflow-run-1","workflowName":"audit","reason":"manual"}}"#,
                "workflow_paused",
                "manual",
            ),
        ];

        for (raw, event_name, detail) in cases {
            let event = map_runtime_event_line(raw).expect("mapped event");
            let value = legacy_json_event(Value::from(7), event);
            assert_eq!(value["event"], event_name);
            assert_eq!(value["taskId"], "task-1");
            assert_eq!(value["runId"], "workflow-run-1");
            assert!(
                value.to_string().contains(detail),
                "mapped event should preserve detail {detail}: {value}"
            );
        }
    }

    #[test]
    fn maps_runtime_session_completed_event() {
        let event = map_runtime_event_line(
            r#"{"type":"session.completed","payload":{"status":"success"}}"#,
        )
        .expect("event");

        assert_eq!(
            event,
            ServerEvent::TurnCompleted {
                status: Value::from("success")
            }
        );
    }
}
