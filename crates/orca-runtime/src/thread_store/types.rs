use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    ActivePermissionProfile, AdditionalWorkingDirectory, PermissionProfileNetworkAccess,
};
use orca_core::conversation::{Message, RawToolCall};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::EventEnvelope;
use orca_core::plan_types::PlanItem;
use orca_core::thread_identity::{ConversationItemId, TurnId};
use orca_core::thread_item_projection::{CompletedModelItem, CompletedModelResponse};
use orca_core::tool_types::{
    ToolInvocationStarted, ToolResultKind, ToolStatus, ToolTerminal, ToolTerminalSource,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::LiveThread;
use crate::history::{CompactionRecord, ContextSummaryRecord};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionMeta {
    pub schema_version: u32,
    pub session_id: String,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub title: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub forked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<ApprovalMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_permission_profile: Option<ActivePermissionProfile>,
    #[serde(default)]
    pub runtime_workspace_roots: Vec<PathBuf>,
    #[serde(default)]
    pub permission_rules: PermissionRules,
    #[serde(default)]
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata_writable_directories: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub path: PathBuf,
    pub archived: bool,
    pub parent_id: Option<String>,
    pub forked: bool,
    pub approval_mode: Option<ApprovalMode>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub runtime_workspace_roots: Vec<PathBuf>,
    pub permission_rule_count: usize,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    pub network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Debug)]
pub struct SessionTranscript {
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    pub compactions: Vec<CompactionRecord>,
    pub summaries: Vec<ContextSummaryRecord>,
    pub usage: Option<UsageTotals>,
    pub plan: Option<(Option<String>, Vec<PlanItem>)>,
    pub completion_status: Option<String>,
    pub completion_error: Option<String>,
    pub next_event_seq: u64,
    pub semantic_events: Vec<EventEnvelope>,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ManualCompactionSnapshotRecord {
    pub snapshot_id: String,
    pub operation_id: crate::runtime_surface::SurfaceOperationId,
    pub strategy: String,
    pub compacted_at: DateTime<Utc>,
    pub before_messages: usize,
    pub messages: Vec<StoredMessage>,
    pub rolling_summary: Option<String>,
    pub summary_state: orca_core::conversation::SummaryState,
}

#[derive(Clone, Debug)]
pub(crate) struct ManualCompactionDurableSnapshot {
    pub operation_id: crate::runtime_surface::SurfaceOperationId,
    pub strategy: String,
    pub before_messages: usize,
    pub conversation: orca_core::conversation::Conversation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub(crate) enum SessionRecord {
    #[serde(rename = "session.meta")]
    Meta(SessionMeta),
    #[serde(rename = "conversation.message")]
    Message {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ConversationItemId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        message: StoredMessage,
    },
    #[serde(rename = "session.completed")]
    Completed {
        status: String,
        completed_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "background_task.provider_response")]
    BackgroundTaskProviderResponse {
        task_id: String,
        status: String,
        completed_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageTotals>,
    },
    #[serde(rename = "context.collapsed")]
    ContextCollapsed(CompactionRecord),
    #[serde(rename = "context.summary")]
    ContextSummary(ContextSummaryRecord),
    #[serde(rename = "context.manual_compaction_snapshot")]
    ManualCompactionSnapshot(ManualCompactionSnapshotRecord),
    #[serde(rename = "session.usage")]
    Usage(UsageTotals),
    #[serde(rename = "session.usage_baseline")]
    UsageBaseline(UsageTotals),
    #[serde(rename = "event.sequence.reserved")]
    EventSequenceReserved { next_seq: u64 },
    #[serde(rename = "event.semantic")]
    SemanticEvent { event: EventEnvelope },
    #[serde(rename = "runtime.surface_commit.prepared")]
    SurfaceCommitPrepared {
        commit_id: String,
        event_count: u32,
        batch_digest: Vec<u8>,
        cursor_before: u64,
        cursor_after: u64,
        durable_revision: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        batch: Option<crate::runtime_surface::StoredSurfaceCommitBatchV1>,
    },
    #[serde(rename = "runtime.surface_commit.committed")]
    SurfaceCommitCommitted {
        commit_id: String,
        event_count: u32,
        batch_digest: Vec<u8>,
        cursor_after: u64,
        durable_revision: u64,
    },
    #[serde(rename = "runtime.surface_owner_epoch")]
    SurfaceOwnerEpoch { owner_epoch: u64 },
    #[serde(rename = "runtime.surface_finalize_intent")]
    SurfaceFinalizeIntent {
        finalize_intent_id: crate::runtime_surface::SurfaceFinalizeIntentId,
        expected_settlements: Vec<crate::runtime_surface::SurfaceSettlementId>,
    },
    #[serde(rename = "runtime.surface_settlement")]
    SurfaceSettlement {
        settlement_id: String,
        receipt_digest: Vec<u8>,
    },
    #[serde(rename = "runtime.surface_shutdown_barrier")]
    SurfaceShutdownBarrier {
        barrier_id: String,
        plan_digest: Vec<u8>,
        record: crate::runtime_surface::StoredShutdownBarrierRecordV1,
    },
    #[serde(rename = "plan.state")]
    PlanState {
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    },
    /// Typed soft-landing checkpoint written when a session stops against a
    /// resumable terminal (for example `budget_exhausted`). It records the
    /// durable boundary a continuation should resume from and the budget
    /// consumed up to that point; it is audit data, not execution state.
    #[serde(rename = "session.checkpoint")]
    Checkpoint(SessionCheckpointRecord),
    #[serde(rename = "provider.prompt_cache_checkpoint")]
    PromptCacheCheckpoint(SessionPromptCacheCheckpointRecord),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionCheckpointRecord {
    pub session_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub budget_consumed: UsageTotals,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_committed_message_id: Option<String>,
    pub resumable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_plan: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SessionPromptCacheCheckpointRecord {
    pub turn_id: TurnId,
    pub checkpoint: orca_provider::prompt_cache::PromptCacheCheckpoint,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredConversationRecord {
    pub(crate) item_id: Option<ConversationItemId>,
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) message: StoredMessage,
    pub(crate) completed_model_items: Option<Vec<CompletedModelItem>>,
}

impl StoredConversationRecord {
    pub(crate) fn legacy(message: StoredMessage) -> Self {
        Self {
            item_id: None,
            turn_id: None,
            message,
            completed_model_items: None,
        }
    }

    pub(crate) fn identified(
        item_id: ConversationItemId,
        turn_id: TurnId,
        message: StoredMessage,
    ) -> Self {
        Self {
            item_id: Some(item_id),
            turn_id: Some(turn_id),
            message,
            completed_model_items: None,
        }
    }

    pub(crate) fn completed_model_response(response: &CompletedModelResponse) -> Self {
        Self {
            item_id: Some(response.conversation_item_id().clone()),
            turn_id: Some(response.turn_id().clone()),
            message: StoredMessage::from(&response.assistant_message()),
            completed_model_items: Some(response.completed_items()),
        }
    }

    pub(crate) fn as_session_record(&self) -> SessionRecord {
        SessionRecord::Message {
            id: self.item_id.clone(),
            turn_id: self.turn_id.clone(),
            message: self.message.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "role")]
pub(crate) enum StoredMessage {
    System {
        content: String,
        #[serde(default)]
        pinned: bool,
    },
    User {
        content: String,
        #[serde(default)]
        pinned: bool,
    },
    Assistant {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
        #[serde(default)]
        pinned: bool,
    },
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(flatten)]
        terminal: StoredToolTerminal,
        #[serde(default)]
        pinned: bool,
    },
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StoredToolTerminal {
    terminal: Option<ToolTerminal>,
}

impl StoredToolTerminal {
    pub(crate) fn from_terminal(terminal: Option<&ToolTerminal>) -> Self {
        Self {
            terminal: terminal.cloned(),
        }
    }

    pub(crate) fn terminal(&self) -> Option<ToolTerminal> {
        self.terminal.clone()
    }

    pub(crate) fn terminal_ref(&self) -> Option<&ToolTerminal> {
        self.terminal.as_ref()
    }

    pub(crate) fn error_mut(&mut self) -> Option<&mut String> {
        self.terminal.as_mut()?.error.as_mut()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct StoredToolTerminalWire {
    #[serde(default)]
    status: Option<ToolStatus>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    kind: Option<ToolResultKind>,
    #[serde(default)]
    terminal_source: Option<ToolTerminalSource>,
    #[serde(default)]
    invocation_started: Option<ToolInvocationStarted>,
}

impl StoredToolTerminalWire {
    fn into_terminal(self) -> Result<Option<ToolTerminal>, String> {
        if let Some(status) = self.status {
            return ToolTerminal::try_from_parts(
                status,
                self.error,
                self.exit_code,
                self.truncated,
                self.kind,
                self.terminal_source.unwrap_or_default(),
                self.invocation_started.unwrap_or_default(),
            )
            .map(Some);
        }
        if self.kind.is_some()
            || self.terminal_source.is_some()
            || self.invocation_started.is_some()
        {
            return Err(
                "stored tool terminal kind/source/start metadata requires status".to_string(),
            );
        }
        if self.error.is_none() && self.exit_code.is_none() && !self.truncated {
            return Ok(None);
        }
        ToolTerminal::try_from_parts(
            ToolStatus::Indeterminate,
            self.error,
            self.exit_code,
            self.truncated,
            Some(ToolResultKind::Indeterminate),
            ToolTerminalSource::CompatibilityRepair,
            ToolInvocationStarted::Unknown,
        )
        .map(Some)
    }
}

pub(crate) fn validate_stored_tool_terminal_fields(message: &Value) -> Option<Result<(), String>> {
    let fields = [
        "status",
        "error",
        "exit_code",
        "truncated",
        "kind",
        "terminal_source",
        "invocation_started",
    ];
    let object = message.as_object()?;
    if !fields.iter().any(|field| object.contains_key(*field)) {
        return None;
    }
    Some(
        serde_json::from_value::<StoredToolTerminalWire>(message.clone())
            .map_err(|error| error.to_string())
            .and_then(|wire| wire.into_terminal().map(|_| ())),
    )
}

impl Serialize for StoredToolTerminal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        if let Some(terminal) = &self.terminal {
            map.serialize_entry("status", &terminal.status)?;
            if let Some(error) = &terminal.error {
                map.serialize_entry("error", error)?;
            }
            if let Some(exit_code) = terminal.exit_code {
                map.serialize_entry("exit_code", &exit_code)?;
            }
            if terminal.truncated {
                map.serialize_entry("truncated", &true)?;
            }
            map.serialize_entry("kind", &terminal.kind)?;
            if terminal.source != ToolTerminalSource::Observed {
                map.serialize_entry("terminal_source", &terminal.source)?;
            }
            if terminal.started != ToolInvocationStarted::Unknown {
                map.serialize_entry("invocation_started", &terminal.started)?;
            }
        }
        map.end()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", tag = "role")]
enum StoredMessageWire {
    System {
        content: String,
        #[serde(default)]
        pinned: bool,
    },
    User {
        content: String,
        #[serde(default)]
        pinned: bool,
    },
    Assistant {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
        #[serde(default)]
        pinned: bool,
    },
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(flatten)]
        terminal: StoredToolTerminalWire,
        #[serde(default)]
        pinned: bool,
    },
}

impl<'de> Deserialize<'de> for StoredMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match StoredMessageWire::deserialize(deserializer)? {
            StoredMessageWire::System { content, pinned } => Ok(Self::System { content, pinned }),
            StoredMessageWire::User { content, pinned } => Ok(Self::User { content, pinned }),
            StoredMessageWire::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } => Ok(Self::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            }),
            StoredMessageWire::Tool {
                tool_call_id,
                content,
                terminal,
                pinned,
            } => {
                let terminal = terminal.into_terminal().map_err(serde::de::Error::custom)?;
                Ok(Self::Tool {
                    tool_call_id,
                    content,
                    terminal: StoredToolTerminal { terminal },
                    pinned,
                })
            }
        }
    }
}

impl From<&Message> for StoredMessage {
    fn from(message: &Message) -> Self {
        match message {
            Message::System { content, pinned } => Self::System {
                content: content.clone(),
                pinned: *pinned,
            },
            Message::User { content, pinned } => Self::User {
                content: content.clone(),
                pinned: *pinned,
            },
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } => Self::Assistant {
                content: content.clone(),
                reasoning_content: reasoning_content.clone(),
                tool_calls: tool_calls.clone(),
                pinned: *pinned,
            },
            Message::Tool {
                tool_call_id,
                content,
                terminal,
                pinned,
            } => Self::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
                terminal: StoredToolTerminal::from_terminal(terminal.as_ref()),
                pinned: *pinned,
            },
        }
    }
}

impl From<StoredMessage> for Message {
    fn from(message: StoredMessage) -> Self {
        match message {
            StoredMessage::System { content, pinned } => Self::System { content, pinned },
            StoredMessage::User { content, pinned } => Self::User { content, pinned },
            StoredMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } => Self::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            },
            StoredMessage::Tool {
                tool_call_id,
                content,
                terminal,
                pinned,
            } => Self::Tool {
                tool_call_id,
                content,
                terminal: terminal.terminal(),
                pinned,
            },
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThreadMetadataPatch {
    pub title: Option<String>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub approval_mode: Option<ApprovalMode>,
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    pub permission_rules: Option<PermissionRules>,
    pub additional_working_directories: Option<Vec<AdditionalWorkingDirectory>>,
    pub metadata_writable_directories: Option<Vec<PathBuf>>,
    pub network_domain_permissions: Option<HashMap<String, PermissionProfileNetworkAccess>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadProjection {
    pub thread_id: String,
    pub title: String,
    pub cwd: String,
    pub runtime_workspace_roots: Vec<PathBuf>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    pub metadata_writable_directories: Vec<PathBuf>,
    pub network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
    pub message_count: usize,
    pub messages: Vec<Value>,
    pub turns: Vec<StoredThreadTurn>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSearchHit {
    pub thread: StoredThreadSummary,
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadTurn {
    pub thread_id: String,
    pub turn_id: String,
    pub index: usize,
    pub role: String,
    pub items_view: TurnItemsView,
    pub items: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadItem {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub index: usize,
    pub item: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadTurnPage {
    pub data: Vec<StoredThreadTurn>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadItemPage {
    pub data: Vec<StoredThreadItem>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSummaryPage {
    pub data: Vec<StoredThreadSummary>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSearchPage {
    pub data: Vec<StoredThreadSearchHit>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadSortKey {
    CreatedAt,
    UpdatedAt,
    RecencyAt,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThreadListFilters {
    pub archived: bool,
    pub model_providers: Option<Vec<String>>,
    pub model_names: Option<Vec<String>>,
    pub cwd_filters: Vec<String>,
    pub relation: Option<ThreadRelationFilter>,
}

impl ThreadListFilters {
    pub fn active() -> Self {
        Self::default()
    }

    pub fn archived() -> Self {
        Self {
            archived: true,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadRelationFilter {
    DirectChildrenOf(String),
    DescendantsOf(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnItemsView {
    NotLoaded,
    Summary,
    Full,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSummary {
    pub thread_id: String,
    pub title: String,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived: bool,
    pub parent_id: Option<String>,
    pub forked: bool,
    pub approval_mode: Option<ApprovalMode>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub permission_rule_count: usize,
    pub runtime_workspace_roots: Vec<PathBuf>,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    pub network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
}

pub trait ThreadStore {
    fn create_live_thread(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<LiveThread>;

    fn create_live_thread_with_permissions(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
        active_permission_profile: Option<ActivePermissionProfile>,
        approval_mode: ApprovalMode,
        permission_rules: PermissionRules,
        additional_working_directories: Vec<AdditionalWorkingDirectory>,
    ) -> io::Result<LiveThread>;

    fn update_thread_metadata(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> io::Result<SessionSummary>;

    fn read_thread(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> io::Result<StoredThreadProjection>;

    fn list_threads(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> io::Result<StoredThreadSummaryPage>;

    fn search_threads(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadSearchPage>;

    fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> io::Result<StoredThreadTurnPage>;

    fn list_thread_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadItemPage>;
}

#[cfg(test)]
mod tests {
    use super::SessionMeta;

    #[test]
    fn legacy_session_metadata_defaults_metadata_write_escalations_to_empty() {
        let metadata: SessionMeta = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "session_id": "legacy-session",
            "cwd": "/workspace",
            "provider": "mock",
            "model": null,
            "title": "legacy",
            "created_at": "2026-07-28T00:00:00Z"
        }))
        .expect("legacy session metadata");

        assert!(metadata.metadata_writable_directories.is_empty());
    }
}
