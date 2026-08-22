//! Runtime-owned continuation boundary for child-agent conversations.
//!
//! This module will host the shared continuation store and child-agent
//! coordinator used by both subagents and workflow child agents. Persistence,
//! checkpoint models, recovery rules, and coordination behavior are added by
//! the follow-up implementation tasks; protocol and surface adapters must not
//! own that state machine.

// T003-T005 intentionally land the shared model before T004-T006 connect its consumers.
#![allow(dead_code)]

use std::{
    collections::{HashMap, HashSet},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use orca_core::budget::BudgetUsage;
use orca_core::config::DelegationSnapshot;
use orca_core::conversation::{
    Conversation, GOAL_CONTEXT_FRAGMENT_ID, GOAL_CONTEXT_MAX_TOKENS, ImageInput,
    InternalContextFragment, InternalContextKind, InternalContextOrigin,
    MEMORY_CONTEXT_FRAGMENT_ID, MEMORY_CONTEXT_MAX_TOKENS, MODE_CONTEXT_FRAGMENT_ID,
    MODE_CONTEXT_MAX_TOKENS, Message, PLAN_CONTEXT_FRAGMENT_ID, PLAN_CONTEXT_MAX_TOKENS,
    RUNTIME_CONTEXT_FRAGMENT_ID, RUNTIME_CONTEXT_MAX_TOKENS, RawToolCall,
    SKILL_CONTEXT_FRAGMENT_ID, SKILL_CONTEXT_MAX_TOKENS, SummaryState, normalize_tool_boundaries,
    repaired_missing_tool_result,
};
use orca_core::external_config::ExternalToolConfig;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::{ToolResultKind, ToolStatus, ToolTerminal, ToolTerminalSource};
use orca_mcp::McpRegistry;
use orca_platform::fs::{AtomicWritePolicy, ExclusiveFileLock, atomic_write};
use serde::{Deserialize, Deserializer, Serialize};

use crate::runtime_surface::Sha256Digest;
use crate::subagent::SubagentIsolation;
use crate::tasks::{TaskRegistry, safe_path_component};

/// Persisted schema version for the first continuation record format.
pub(crate) const AGENT_CONTINUATION_SCHEMA_VERSION: u32 = 1;

/// Persisted schema version for child conversation snapshots.
pub(crate) const CHILD_CONVERSATION_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Canonical checkpoint digest payload version.
pub(crate) const AGENT_CHECKPOINT_PAYLOAD_SCHEMA_VERSION: u32 = 1;

/// Fixed execution-lease lifetime used by child continuation attempts.
const CONTINUATION_LEASE_DURATION_MS: i64 = 30_000;
const CONTINUATION_NOT_FOUND_CODE: &str = "continuation_not_found";
const CONTINUATION_PARENT_MISMATCH_CODE: &str = "continuation_parent_mismatch";
const CONTINUATION_ACTIVE_CODE: &str = "continuation_active";
const CONTINUATION_INCOMPATIBLE_CODE: &str = "continuation_incompatible";
const CONTINUATION_CHECKPOINT_MISSING_CODE: &str = "continuation_checkpoint_missing";
const CONTINUATION_CHECKPOINT_CORRUPT_CODE: &str = "continuation_checkpoint_corrupt";
const CONTINUATION_INDETERMINATE_CODE: &str = "continuation_indeterminate";
const CONTINUATION_REVISION_CONFLICT_CODE: &str = "continuation_revision_conflict";
const CONTINUATION_ALREADY_EXISTS_CODE: &str = "continuation_already_exists";
const CONTINUATION_PERSISTENCE_ERROR_CODE: &str = "continuation_persistence_error";

macro_rules! uuid_v7_id {
    ($name:ident, $kind:ident, $label:literal) => {
        #[doc = concat!("Stable UUIDv7 identity for an agent ", $label, ".")]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);

        impl $name {
            #[doc = concat!("Generates a new agent ", $label, " identity.")]
            pub(crate) fn new() -> Self {
                Self(uuid::Uuid::now_v7().hyphenated().to_string())
            }

            #[doc = concat!("Parses an agent ", $label, " identity and rejects non-v7 UUIDs.")]
            pub(crate) fn parse(value: impl Into<String>) -> Result<Self, AgentContinuationError> {
                let value = value.into();
                let parsed = uuid::Uuid::parse_str(&value).map_err(|_| {
                    AgentContinuationError::InvalidUuid {
                        kind: ContinuationIdKind::$kind,
                    }
                })?;
                if parsed.get_version_num() != 7 {
                    return Err(AgentContinuationError::WrongUuidVersion {
                        kind: ContinuationIdKind::$kind,
                        found: parsed.get_version_num(),
                    });
                }
                Ok(Self(value))
            }

            #[doc = concat!("Returns the serialized agent ", $label, " identity.")]
            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

uuid_v7_id!(AgentContinuationId, Continuation, "continuation");
uuid_v7_id!(AgentAttemptId, Attempt, "attempt");
uuid_v7_id!(AgentCheckpointId, Checkpoint, "checkpoint");
uuid_v7_id!(AgentPromptId, Prompt, "prompt");

/// Monotonic compare-and-swap revision for one continuation record.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct ContinuationRevision(u64);

impl ContinuationRevision {
    /// Initial revision before the first committed mutation.
    pub(crate) const ZERO: Self = Self(0);

    /// Creates a typed revision from its persisted integer value.
    pub(crate) const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    /// Returns the persisted integer value.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    /// Returns the exact successor revision or fails if the counter is exhausted.
    pub(crate) fn next(self) -> Result<Self, AgentContinuationError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(AgentContinuationError::RevisionExhausted)
    }
}

/// Authoritative lifecycle state for a durable child-agent continuation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContinuationStatus {
    /// The lineage exists but has not acquired an execution lease.
    #[default]
    Created,
    /// The current attempt is executing.
    Running,
    /// A safe checkpoint has been committed while execution may continue.
    Checkpointed,
    /// Execution stopped at a safe checkpoint and awaits explicit resumption.
    Suspended,
    /// A new attempt is being prepared from a safe checkpoint.
    Resuming,
    /// The latest attempt completed successfully.
    Completed,
    /// The latest attempt failed with a known terminal outcome.
    Failed,
    /// The latest attempt was cancelled.
    Cancelled,
    /// An external side effect may have started without a trustworthy terminal result.
    Indeterminate,
}

impl ContinuationStatus {
    /// Applies a valid continuation state transition and rejects all other edges.
    pub(crate) fn transition_to(&mut self, next: Self) -> Result<(), AgentContinuationError> {
        let allowed = matches!(
            (*self, next),
            (
                Self::Created,
                Self::Running | Self::Failed | Self::Cancelled
            ) | (
                Self::Running,
                Self::Checkpointed
                    | Self::Completed
                    | Self::Failed
                    | Self::Cancelled
                    | Self::Indeterminate
            ) | (
                Self::Checkpointed,
                Self::Running
                    | Self::Suspended
                    | Self::Completed
                    | Self::Failed
                    | Self::Cancelled
                    | Self::Indeterminate
            ) | (Self::Suspended, Self::Resuming | Self::Cancelled)
                | (
                    Self::Resuming,
                    Self::Running | Self::Failed | Self::Cancelled | Self::Indeterminate
                )
                | (
                    Self::Completed | Self::Failed | Self::Cancelled,
                    Self::Resuming
                )
        );
        if !allowed {
            return Err(AgentContinuationError::InvalidTransition {
                from: *self,
                to: next,
            });
        }
        *self = next;
        Ok(())
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Checkpointed => "checkpointed",
            Self::Suspended => "suspended",
            Self::Resuming => "resuming",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Indeterminate => "indeterminate",
        }
    }
}

/// Lifecycle state for one concrete execution attempt.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttemptState {
    /// The attempt is durable but does not yet hold an execution lease.
    #[default]
    Prepared,
    /// The attempt currently owns execution.
    Running,
    /// The attempt stopped at a safe checkpoint.
    Suspended,
    /// The attempt has a durable terminal outcome.
    Terminal,
}

impl AttemptState {
    /// Applies a valid attempt state transition and rejects all other edges.
    pub(crate) fn transition_to(&mut self, next: Self) -> Result<(), AgentContinuationError> {
        let allowed = matches!(
            (*self, next),
            (Self::Prepared, Self::Running | Self::Terminal)
                | (Self::Running, Self::Suspended | Self::Terminal)
                | (Self::Suspended, Self::Running | Self::Terminal)
        );
        if !allowed {
            return Err(AgentContinuationError::InvalidAttemptTransition {
                from: *self,
                to: next,
            });
        }
        *self = next;
        Ok(())
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Terminal => "terminal",
        }
    }
}

/// Durable binding for a child-agent worktree that may be resumed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WorktreeBinding {
    /// Repository root used to create the worktree.
    pub(crate) repo_root: String,
    /// Effective worktree path used by the child agent.
    pub(crate) path: String,
}

/// Current execution and lease metadata for one attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentAttemptRecord {
    /// Stable identity of this execution attempt.
    pub(crate) attempt_id: AgentAttemptId,
    /// Attempt whose checkpoint seeded this attempt, when resuming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resumed_from_attempt_id: Option<AgentAttemptId>,
    /// Current runtime owner of the lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner_id: Option<String>,
    /// Monotonic lease fencing epoch.
    #[serde(default)]
    pub(crate) lease_epoch: u64,
    /// Diagnostic lease expiry in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lease_expires_at_ms: Option<i64>,
    /// Durable lifecycle state for this attempt.
    #[serde(default)]
    pub(crate) state: AttemptState,
    /// Stable idempotency identity for the accepted prompt.
    pub(crate) prompt_id: AgentPromptId,
    /// Start time in Unix milliseconds, if execution began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) started_at_ms: Option<i64>,
    /// Completion time in Unix milliseconds, if the attempt settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completed_at_ms: Option<i64>,
}

/// Versioned, serde-compatible snapshot of one child-agent conversation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ChildConversationSnapshot {
    /// Snapshot wire schema version.
    #[serde(default = "default_child_conversation_snapshot_schema_version")]
    pub(crate) schema_version: u32,
    /// Normalized non-system conversation history.
    #[serde(default)]
    pub(crate) messages: Vec<StoredChildMessage>,
    /// Latest rolling summary used by context compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rolling_summary: Option<String>,
    /// Baseline and delta summary state.
    #[serde(default)]
    pub(crate) summary: StoredChildSummaryState,
    /// Bounded child-loop context fragments with stable runtime meanings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) internal_context: Vec<StoredChildInternalContextFragment>,
    /// Turn cursor to use for the next child-loop iteration.
    #[serde(default)]
    pub(crate) next_turn: u32,
}

impl ChildConversationSnapshot {
    fn capture_unchecked(conversation: &Conversation, next_turn: u32) -> Self {
        let mut messages = conversation
            .messages
            .iter()
            .filter(|message| !matches!(message, Message::System { .. }))
            .cloned()
            .collect::<Vec<_>>();
        normalize_tool_boundaries(&mut messages);
        repair_missing_tool_terminals(&mut messages);

        Self {
            schema_version: CHILD_CONVERSATION_SNAPSHOT_SCHEMA_VERSION,
            messages: messages
                .iter()
                .map(StoredChildMessage::from_message)
                .collect(),
            rolling_summary: conversation.rolling_summary.clone(),
            summary: StoredChildSummaryState::from(&conversation.summary),
            internal_context: conversation
                .internal_context
                .fragments()
                .iter()
                .filter_map(StoredChildInternalContextFragment::from_fragment)
                .collect(),
            next_turn,
        }
    }

    /// Captures only a conversation whose tool calls already have trustworthy
    /// terminal facts; it returns the latest settled tool boundary alongside
    /// the snapshot and fails closed before normalization can repair an unsafe
    /// boundary into apparently resumable history.
    pub(crate) fn try_capture_safe(
        conversation: &Conversation,
        next_turn: u32,
    ) -> Result<(Self, Option<ToolBoundary>), AgentContinuationError> {
        let last_tool_boundary = try_last_settled_tool_boundary(conversation)?;
        Ok((
            Self::capture_unchecked(conversation, next_turn),
            last_tool_boundary,
        ))
    }

    /// Restores this snapshot after fresh system prompts have been added by the caller.
    pub(crate) fn restore_into(
        &self,
        conversation: &mut Conversation,
    ) -> Result<u32, AgentContinuationError> {
        if self.schema_version != CHILD_CONVERSATION_SNAPSHOT_SCHEMA_VERSION {
            return Err(AgentContinuationError::UnsupportedSchemaVersion {
                found: self.schema_version,
            });
        }
        if conversation
            .messages
            .iter()
            .any(|message| !matches!(message, Message::System { .. }))
            || !conversation.internal_context.is_empty()
            || conversation.rolling_summary.is_some()
            || !conversation.summary.is_empty()
        {
            return Err(AgentContinuationError::CorruptRecord {
                message:
                    "child conversation restore target must contain only fresh system messages"
                        .to_string(),
            });
        }

        let mut restored_messages = self
            .messages
            .iter()
            .map(StoredChildMessage::to_message)
            .collect::<Vec<_>>();
        normalize_tool_boundaries(&mut restored_messages);
        repair_missing_tool_terminals(&mut restored_messages);
        conversation.messages.extend(restored_messages);
        conversation.rolling_summary = self.rolling_summary.clone();
        conversation.summary = SummaryState::from(&self.summary);
        for fragment in &self.internal_context {
            conversation
                .internal_context
                .replace(fragment.to_fragment());
        }

        Ok(self.next_turn)
    }
}

pub(crate) fn conversation_has_open_tool_calls(conversation: &Conversation) -> bool {
    let mut open = HashSet::new();
    for message in &conversation.messages {
        match message {
            Message::Assistant { tool_calls, .. } => {
                open.extend(tool_calls.iter().map(|tool_call| tool_call.id.as_str()));
            }
            Message::Tool {
                tool_call_id,
                terminal: Some(_),
                ..
            } => {
                open.remove(tool_call_id.as_str());
            }
            Message::Tool { .. } | Message::System { .. } | Message::User { .. } => {}
        }
    }
    !open.is_empty()
}

/// Validates one conversation as a checkpoint-safe boundary and returns the
/// latest trustworthy terminal tool identity. Open calls, repaired terminals,
/// and indeterminate status/kind fail closed without changing conversation.
pub(crate) fn try_last_settled_tool_boundary(
    conversation: &Conversation,
) -> Result<Option<ToolBoundary>, AgentContinuationError> {
    let mut open = HashSet::new();
    let mut last_tool_boundary = None;

    for message in &conversation.messages {
        match message {
            Message::Assistant { tool_calls, .. } => {
                if tool_calls.is_empty() {
                    last_tool_boundary = None;
                }
                for tool_call in tool_calls {
                    if !open.insert(tool_call.id.as_str()) {
                        return Err(AgentContinuationError::CorruptRecord {
                            message: format!(
                                "child conversation repeats open tool call '{}'",
                                tool_call.id
                            ),
                        });
                    }
                }
            }
            Message::Tool {
                tool_call_id,
                terminal,
                ..
            } => {
                if !open.remove(tool_call_id.as_str()) {
                    return Err(AgentContinuationError::CorruptRecord {
                        message: format!(
                            "child conversation has a tool result without open call '{}'",
                            tool_call_id
                        ),
                    });
                }
                let Some(terminal) = terminal else {
                    return Err(AgentContinuationError::Indeterminate);
                };
                if terminal.status == ToolStatus::Indeterminate
                    || terminal.kind == ToolResultKind::Indeterminate
                    || terminal.source != ToolTerminalSource::Observed
                {
                    return Err(AgentContinuationError::Indeterminate);
                }
                last_tool_boundary = Some(ToolBoundary::Completed {
                    tool_call_id: Some(tool_call_id.clone()),
                });
            }
            Message::User { .. } => last_tool_boundary = None,
            Message::System { .. } => {}
        }
    }

    if !open.is_empty() {
        return Err(AgentContinuationError::Indeterminate);
    }
    Ok(last_tool_boundary)
}

fn repair_missing_tool_terminals(messages: &mut [Message]) {
    let tool_calls = messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { tool_calls, .. } => Some(tool_calls.as_slice()),
            _ => None,
        })
        .flatten()
        .map(|tool_call| (tool_call.id.clone(), tool_call.clone()))
        .collect::<HashMap<_, _>>();

    for message in messages {
        let Message::Tool {
            tool_call_id,
            terminal,
            ..
        } = message
        else {
            continue;
        };
        if terminal.is_some() {
            continue;
        }
        let Some(tool_call) = tool_calls.get(tool_call_id) else {
            continue;
        };
        let Message::Tool {
            terminal: repaired_terminal,
            ..
        } = repaired_missing_tool_result(tool_call)
        else {
            unreachable!("tool repair always creates a tool result")
        };
        *terminal = repaired_terminal;
    }
}

/// Persisted child message roles; system prompts are intentionally absent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "role")]
pub(crate) enum StoredChildMessage {
    /// User-authored child prompt or follow-up.
    User {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageInput>,
        #[serde(default)]
        pinned: bool,
    },
    /// Assistant output, reasoning replay data, and raw tool calls.
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<RawToolCall>,
        #[serde(default)]
        pinned: bool,
    },
    /// Tool result and its trustworthy or repaired terminal metadata.
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal: Option<ToolTerminal>,
        #[serde(default)]
        pinned: bool,
    },
}

impl StoredChildMessage {
    fn from_message(message: &Message) -> Self {
        match message {
            Message::User {
                content,
                images,
                pinned,
            } => Self::User {
                content: content.clone(),
                images: images.clone(),
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
                terminal: terminal.clone(),
                pinned: *pinned,
            },
            Message::System { .. } => {
                unreachable!("system messages are filtered before child snapshot conversion")
            }
        }
    }

    fn to_message(&self) -> Message {
        match self {
            Self::User {
                content,
                images,
                pinned,
            } => Message::User {
                content: content.clone(),
                images: images.clone(),
                pinned: *pinned,
            },
            Self::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } => Message::Assistant {
                content: content.clone(),
                reasoning_content: reasoning_content.clone(),
                tool_calls: tool_calls.clone(),
                pinned: *pinned,
            },
            Self::Tool {
                tool_call_id,
                content,
                terminal,
                pinned,
            } => Message::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
                terminal: terminal.clone(),
                pinned: *pinned,
            },
        }
    }
}

/// Serde wire for the child conversation summary state.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct StoredChildSummaryState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baseline: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deltas: Vec<String>,
}

impl From<&SummaryState> for StoredChildSummaryState {
    fn from(summary: &SummaryState) -> Self {
        Self {
            baseline: summary.baseline.clone(),
            deltas: summary.deltas.clone(),
        }
    }
}

impl From<&StoredChildSummaryState> for SummaryState {
    fn from(summary: &StoredChildSummaryState) -> Self {
        Self {
            baseline: summary.baseline.clone(),
            deltas: summary.deltas.clone(),
        }
    }
}

/// Restricted internal context fragments eligible for child continuation persistence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum StoredChildInternalContextFragment {
    Runtime { content: String },
    Mode { content: String },
    Goal { content: String },
    Plan { content: String },
    Skill { content: String },
    Memory { content: String },
}

impl StoredChildInternalContextFragment {
    fn from_fragment(fragment: &InternalContextFragment) -> Option<Self> {
        let content = fragment.content.clone();
        match fragment.id.as_str() {
            RUNTIME_CONTEXT_FRAGMENT_ID => Some(Self::Runtime { content }),
            MODE_CONTEXT_FRAGMENT_ID => Some(Self::Mode { content }),
            GOAL_CONTEXT_FRAGMENT_ID => Some(Self::Goal { content }),
            PLAN_CONTEXT_FRAGMENT_ID => Some(Self::Plan { content }),
            SKILL_CONTEXT_FRAGMENT_ID => Some(Self::Skill { content }),
            MEMORY_CONTEXT_FRAGMENT_ID => Some(Self::Memory { content }),
            _ => None,
        }
    }

    fn to_fragment(&self) -> InternalContextFragment {
        let (id, kind, origin, content, max_tokens) = match self {
            Self::Runtime { content } => (
                RUNTIME_CONTEXT_FRAGMENT_ID,
                InternalContextKind::Runtime,
                InternalContextOrigin::System,
                content,
                RUNTIME_CONTEXT_MAX_TOKENS,
            ),
            Self::Mode { content } => (
                MODE_CONTEXT_FRAGMENT_ID,
                InternalContextKind::Runtime,
                InternalContextOrigin::System,
                content,
                MODE_CONTEXT_MAX_TOKENS,
            ),
            Self::Goal { content } => (
                GOAL_CONTEXT_FRAGMENT_ID,
                InternalContextKind::Goal,
                InternalContextOrigin::GoalRuntime,
                content,
                GOAL_CONTEXT_MAX_TOKENS,
            ),
            Self::Plan { content } => (
                PLAN_CONTEXT_FRAGMENT_ID,
                InternalContextKind::Plan,
                InternalContextOrigin::Model,
                content,
                PLAN_CONTEXT_MAX_TOKENS,
            ),
            Self::Skill { content } => (
                SKILL_CONTEXT_FRAGMENT_ID,
                InternalContextKind::Skill,
                InternalContextOrigin::User,
                content,
                SKILL_CONTEXT_MAX_TOKENS,
            ),
            Self::Memory { content } => (
                MEMORY_CONTEXT_FRAGMENT_ID,
                InternalContextKind::Memory,
                InternalContextOrigin::System,
                content,
                MEMORY_CONTEXT_MAX_TOKENS,
            ),
        };
        InternalContextFragment {
            id: id.to_string(),
            kind,
            origin,
            content: content.clone(),
            max_tokens,
        }
    }
}

/// Replay disposition at the latest durable tool boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ToolBoundary {
    /// A trustworthy terminal tool result is present and must be retained.
    Completed {
        /// Stable tool call identity, when one was assigned.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    /// The tool did not start or explicitly permits safe retry.
    SafeToRetry {
        /// Stable tool call identity, when one was assigned.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    /// Replay is allowed only with the same stable idempotency key.
    IdempotentWithKey {
        /// Stable tool call identity.
        tool_call_id: String,
        /// Key that must be reused for a replay or receipt lookup.
        idempotency_key: String,
    },
    /// The tool may have started without a trustworthy terminal result.
    Indeterminate {
        /// Stable tool call identity, when one was assigned.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        /// Stable diagnostic reason for blocking automatic replay.
        reason: String,
    },
}

/// Metadata for the latest safe, durable child conversation checkpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentCheckpoint {
    /// Stable checkpoint identity.
    pub(crate) checkpoint_id: AgentCheckpointId,
    /// Attempt that produced this checkpoint.
    pub(crate) attempt_id: AgentAttemptId,
    /// Monotonically increasing checkpoint sequence within the continuation.
    #[serde(default)]
    pub(crate) sequence: u64,
    /// Complete durable child conversation payload for this checkpoint.
    pub(crate) conversation: ChildConversationSnapshot,
    /// Number of completed child turns represented by the checkpoint.
    #[serde(default)]
    pub(crate) turn: u32,
    /// Cumulative budget consumed through this checkpoint.
    #[serde(default)]
    pub(crate) usage: BudgetUsage,
    /// Latest settled tool boundary relevant to safe recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_tool_boundary: Option<ToolBoundary>,
    /// Checkpoint creation time in Unix milliseconds.
    pub(crate) created_at_ms: i64,
    /// SHA-256 digest of the canonical checkpoint payload.
    pub(crate) digest: Sha256Digest,
}

impl AgentCheckpoint {
    /// Serializes the stable checkpoint digest input without including the digest itself.
    pub(crate) fn canonical_payload_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&CanonicalAgentCheckpointPayload {
            schema_version: AGENT_CHECKPOINT_PAYLOAD_SCHEMA_VERSION,
            checkpoint_id: &self.checkpoint_id,
            attempt_id: &self.attempt_id,
            sequence: self.sequence,
            conversation: &self.conversation,
            turn: self.turn,
            usage: &self.usage,
            last_tool_boundary: self.last_tool_boundary.as_ref(),
            created_at_ms: self.created_at_ms,
        })
    }

    /// Computes the digest required when creating or committing this checkpoint.
    pub(crate) fn computed_digest(&self) -> Result<Sha256Digest, AgentContinuationError> {
        self.canonical_payload_bytes()
            .map(Sha256Digest::digest)
            .map_err(|_| AgentContinuationError::Persistence {
                message: "failed to serialize checkpoint digest payload".to_string(),
            })
    }

    /// Verifies that the persisted digest matches the canonical checkpoint payload.
    pub(crate) fn verify_digest(&self) -> Result<(), AgentContinuationError> {
        if self.computed_digest()? != self.digest {
            return Err(AgentContinuationError::DigestMismatch);
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct CanonicalAgentCheckpointPayload<'a> {
    schema_version: u32,
    checkpoint_id: &'a AgentCheckpointId,
    attempt_id: &'a AgentAttemptId,
    sequence: u64,
    conversation: &'a ChildConversationSnapshot,
    turn: u32,
    usage: &'a BudgetUsage,
    last_tool_boundary: Option<&'a ToolBoundary>,
    created_at_ms: i64,
}

/// Durable terminal outcome for the latest attempt in a continuation lineage.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum AgentTerminal {
    /// The attempt completed successfully.
    Completed {
        /// Optional final assistant result retained for projection or cache use.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// The attempt failed with a known error.
    Failed {
        /// Stable user-facing failure message.
        error: String,
    },
    /// The attempt was cancelled before natural completion.
    Cancelled {
        /// Optional stable cancellation reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The attempt stopped with an unknown external side-effect outcome.
    Indeterminate {
        /// Stable reason automatic replay is prohibited.
        reason: String,
    },
}

/// Complete durable continuation metadata owned by the runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentContinuationRecord {
    /// Persistence schema version.
    #[serde(default = "default_schema_version")]
    pub(crate) schema_version: u32,
    /// Stable continuation lineage identity.
    pub(crate) continuation_id: AgentContinuationId,
    /// Parent session that exclusively owns this lineage.
    pub(crate) parent_session_id: String,
    /// Parent task, when the lineage was launched from another task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_task_id: Option<String>,
    /// Task that first created the lineage.
    pub(crate) source_task_id: String,
    /// Task associated with the current attempt.
    pub(crate) latest_task_id: String,
    /// Stable child-agent type identity.
    pub(crate) subagent_type: String,
    /// Fixed or inherited model identity used for compatibility checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    /// Isolation policy used by the child agent.
    pub(crate) isolation: SubagentIsolation,
    /// Effective child working directory.
    pub(crate) effective_cwd: String,
    /// Durable worktree binding, when worktree isolation is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) worktree: Option<WorktreeBinding>,
    /// Compatibility digest for agent, model, policy, tools, and workspace identity.
    pub(crate) compatibility_hash: Sha256Digest,
    /// Compare-and-swap revision for all durable mutations.
    #[serde(default)]
    pub(crate) revision: ContinuationRevision,
    /// Authoritative continuation lifecycle state.
    #[serde(default)]
    pub(crate) status: ContinuationStatus,
    /// Current execution attempt and lease metadata.
    pub(crate) current_attempt: AgentAttemptRecord,
    /// Latest safe checkpoint metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checkpoint: Option<AgentCheckpoint>,
    /// Tool invocation admitted after the latest checkpoint but not yet
    /// covered by a newer safe conversation checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active_tool_boundary: Option<ToolBoundary>,
    /// Latest terminal lineage outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal: Option<AgentTerminal>,
    /// Creation time in Unix milliseconds.
    pub(crate) created_at_ms: i64,
    /// Last durable update time in Unix milliseconds.
    pub(crate) updated_at_ms: i64,
}

/// Compatibility and workspace identity that must remain stable across attempts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ContinuationCompatibility {
    /// Stable child-agent type identity.
    pub(crate) subagent_type: String,
    /// Effective model identity after runtime inheritance is resolved.
    pub(crate) model: Option<String>,
    /// Isolation policy used by the child agent.
    pub(crate) isolation: SubagentIsolation,
    /// Effective child working directory.
    pub(crate) effective_cwd: String,
    /// Durable worktree binding, when worktree isolation is active.
    pub(crate) worktree: Option<WorktreeBinding>,
    /// Digest covering model, policy, tools, and workspace identity.
    pub(crate) compatibility_hash: Sha256Digest,
}

/// Computes the stable child-runtime compatibility digest from the effective
/// agent, model, policy, tool catalog, and workspace bindings. Inputs are
/// serialized with sorted object keys and a fixed field order; serialization
/// failures are returned without creating or mutating continuation state.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_continuation_compatibility_hash(
    subagent_type: &SubagentType,
    model: Option<&str>,
    isolation: SubagentIsolation,
    effective_cwd: &str,
    worktree: Option<&WorktreeBinding>,
    delegation: &DelegationSnapshot,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Result<Sha256Digest, AgentContinuationError> {
    let mut mcp_tools = mcp_registry.tools().iter().collect::<Vec<_>>();
    mcp_tools.sort_by(|left, right| {
        (&left.server, &left.schema_name, &left.name).cmp(&(
            &right.server,
            &right.schema_name,
            &right.name,
        ))
    });
    let mut external_tools = external_tools
        .iter()
        .map(|tool| {
            let identity = serde_json::json!({
                "name": tool.name,
                "action_kind": tool.action_kind,
                "command": tool.command,
                "description": tool.description,
                "schema": tool.schema,
            });
            canonical_json_bytes(&identity).map(|stable_key| (stable_key, identity))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AgentContinuationError::Persistence {
            message: "failed to serialize external tool compatibility schema".to_string(),
        })?;
    external_tools.sort_by(|(left, _), (right, _)| left.cmp(right));
    let external_tools = external_tools
        .into_iter()
        .map(|(_, identity)| identity)
        .collect::<Vec<_>>();
    let payload = serde_json::json!({
        "schema_version": 1,
        "subagent_type": subagent_type,
        "model": model,
        "isolation": isolation,
        "effective_cwd": effective_cwd,
        "worktree": worktree,
        "delegation": delegation,
        "mcp_tools": mcp_tools,
        "external_tools": external_tools,
    });
    canonical_json_bytes(&payload)
        .map(Sha256Digest::digest)
        .map_err(|_| AgentContinuationError::Persistence {
            message: "failed to serialize continuation compatibility payload".to_string(),
        })
}

fn canonical_json_bytes(value: &serde_json::Value) -> Result<Vec<u8>, serde_json::Error> {
    fn write_value(
        output: &mut Vec<u8>,
        value: &serde_json::Value,
    ) -> Result<(), serde_json::Error> {
        match value {
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => output.extend(serde_json::to_vec(value)?),
            serde_json::Value::Array(values) => {
                output.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    write_value(output, value)?;
                }
                output.push(b']');
            }
            serde_json::Value::Object(values) => {
                output.push(b'{');
                let mut entries = values.iter().collect::<Vec<_>>();
                entries.sort_by_key(|(key, _)| *key);
                for (index, (key, value)) in entries.into_iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    output.extend(serde_json::to_vec(key)?);
                    output.push(b':');
                    write_value(output, value)?;
                }
                output.push(b'}');
            }
        }
        Ok(())
    }

    let mut output = Vec::new();
    write_value(&mut output, value)?;
    Ok(output)
}

/// Input for creating a new child-agent continuation lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CreateContinuationInput {
    /// Optional caller-supplied identity; normal callers leave this unset.
    pub(crate) continuation_id: Option<AgentContinuationId>,
    /// Parent task that owns this child lineage, when one exists.
    pub(crate) parent_task_id: Option<String>,
    /// Task associated with the first attempt.
    pub(crate) task_id: String,
    /// Stable prompt idempotency identity for the first attempt.
    pub(crate) prompt_id: AgentPromptId,
    /// Compatibility identity fixed for this lineage.
    pub(crate) compatibility: ContinuationCompatibility,
}

/// Input for preparing a new attempt from an existing continuation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResumeContinuationInput {
    /// Continuation UUIDv7 or compatibility task/agent selector.
    pub(crate) selector: String,
    /// Parent task that must still own this child lineage.
    pub(crate) parent_task_id: Option<String>,
    /// Task associated with the newly prepared attempt.
    pub(crate) task_id: String,
    /// Stable prompt idempotency identity for the resume request.
    pub(crate) prompt_id: AgentPromptId,
    /// Runtime compatibility identity required by the new attempt.
    pub(crate) compatibility: ContinuationCompatibility,
}

/// Durable startup facts returned to sync and async child-agent launchers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedContinuation {
    /// Stable continuation lineage identity.
    pub(crate) continuation_id: AgentContinuationId,
    /// Current attempt identity, reused for idempotent duplicate requests.
    pub(crate) attempt_id: AgentAttemptId,
    /// Prompt idempotency identity accepted by the current attempt.
    pub(crate) prompt_id: AgentPromptId,
    /// Compatibility and workspace identity the child loop must retain.
    pub(crate) compatibility: ContinuationCompatibility,
    /// Source checkpoint for resume, or the latest checkpoint after settlement.
    pub(crate) checkpoint: Option<AgentCheckpoint>,
    /// Current durable revision to use when acquiring or committing.
    pub(crate) revision: ContinuationRevision,
    /// Task that first created this lineage.
    pub(crate) source_task_id: String,
    /// Task currently bound to this attempt.
    pub(crate) latest_task_id: String,
    /// Parent task binding retained across attempts.
    pub(crate) parent_task_id: Option<String>,
    /// Existing terminal returned for an idempotent duplicate prompt.
    pub(crate) terminal: Option<AgentTerminal>,
}

impl From<&AgentContinuationRecord> for PreparedContinuation {
    fn from(record: &AgentContinuationRecord) -> Self {
        Self {
            continuation_id: record.continuation_id.clone(),
            attempt_id: record.current_attempt.attempt_id.clone(),
            prompt_id: record.current_attempt.prompt_id.clone(),
            compatibility: ContinuationCompatibility {
                subagent_type: record.subagent_type.clone(),
                model: record.model.clone(),
                isolation: record.isolation,
                effective_cwd: record.effective_cwd.clone(),
                worktree: record.worktree.clone(),
                compatibility_hash: record.compatibility_hash,
            },
            checkpoint: record.checkpoint.clone(),
            revision: record.revision,
            source_task_id: record.source_task_id.clone(),
            latest_task_id: record.latest_task_id.clone(),
            parent_task_id: record.parent_task_id.clone(),
            terminal: record.terminal.clone(),
        }
    }
}

/// Session-scoped durable or process-local storage for child-agent continuations.
#[derive(Clone)]
pub(crate) struct AgentContinuationStore {
    parent_session_id: String,
    backend: AgentContinuationStoreBackend,
}

#[derive(Clone)]
enum AgentContinuationStoreBackend {
    ProcessLocal {
        state: Arc<Mutex<ProcessLocalContinuationState>>,
    },
    Persistent {
        root: PathBuf,
        cache: Arc<Mutex<HashMap<AgentContinuationId, AgentContinuationRecord>>>,
    },
}

pub(crate) struct ContinuationReconciliation {
    projections: Vec<ContinuationProjection>,
    _session_lock: Option<ExclusiveFileLock>,
}

impl ContinuationReconciliation {
    pub(crate) fn projections(&self) -> &[ContinuationProjection] {
        &self.projections
    }

    fn into_projections(self) -> Vec<ContinuationProjection> {
        self.projections
    }
}

#[derive(Default)]
struct ProcessLocalContinuationState {
    index: HashMap<AgentContinuationId, String>,
    records: HashMap<AgentContinuationId, AgentContinuationRecord>,
}

impl AgentContinuationStore {
    /// Creates a process-local store whose clones share records and fencing state.
    pub(crate) fn new(parent_session_id: String) -> Self {
        Self {
            parent_session_id,
            backend: AgentContinuationStoreBackend::ProcessLocal {
                state: Arc::new(Mutex::new(ProcessLocalContinuationState::default())),
            },
        }
    }

    /// Opens a persistent store under the task-session root for one parent session.
    pub(crate) fn new_persistent(
        parent_session_id: String,
        root: PathBuf,
    ) -> Result<Self, AgentContinuationError> {
        fs::create_dir_all(&root)
            .map_err(|error| persistence_io_error("create continuation storage root", &error))?;
        Ok(Self {
            parent_session_id,
            backend: AgentContinuationStoreBackend::Persistent {
                root,
                cache: Arc::new(Mutex::new(HashMap::new())),
            },
        })
    }

    /// Returns the parent session whose continuations this store may access.
    pub(crate) fn parent_session_id(&self) -> &str {
        &self.parent_session_id
    }

    /// Creates one new revision-zero continuation after validating all durable invariants.
    pub(crate) fn create_record(
        &self,
        record: AgentContinuationRecord,
    ) -> Result<AgentContinuationRecord, AgentContinuationError> {
        if record.revision != ContinuationRevision::ZERO {
            return Err(corrupt_record("new continuation revision must be zero"));
        }
        validate_record(
            &record,
            Some(&record.continuation_id),
            Some(&self.parent_session_id),
        )?;

        match &self.backend {
            AgentContinuationStoreBackend::ProcessLocal { state } => {
                let mut state = lock_process_state(state)?;
                if let Some(parent_session_id) = state.index.get(&record.continuation_id) {
                    if parent_session_id != &self.parent_session_id {
                        return Err(AgentContinuationError::ParentMismatch {
                            expected: self.parent_session_id.clone(),
                            actual: parent_session_id.clone(),
                        });
                    }
                    return Err(AgentContinuationError::AlreadyExists {
                        continuation_id: record.continuation_id,
                    });
                }
                state.index.insert(
                    record.continuation_id.clone(),
                    self.parent_session_id.clone(),
                );
                state
                    .records
                    .insert(record.continuation_id.clone(), record.clone());
                Ok(record)
            }
            AgentContinuationStoreBackend::Persistent { root, cache } => {
                let _session_lock = acquire_lock(
                    &session_lock_path(root, &self.parent_session_id),
                    "acquire continuation session lock",
                )?;
                let _index_lock = acquire_lock(
                    &continuation_index_lock_path(root),
                    "acquire continuation index lock",
                )?;
                let mut index = load_continuation_index(root)?;
                if let Some(parent_session_id) = index.get(record.continuation_id.as_str()) {
                    if parent_session_id != &self.parent_session_id {
                        return Err(AgentContinuationError::ParentMismatch {
                            expected: self.parent_session_id.clone(),
                            actual: parent_session_id.clone(),
                        });
                    }
                    let existing =
                        read_record(root, &self.parent_session_id, &record.continuation_id)?;
                    if existing != record {
                        return Err(AgentContinuationError::AlreadyExists {
                            continuation_id: record.continuation_id,
                        });
                    }
                    cache_record(cache, existing.clone())?;
                    return Ok(existing);
                }
                let record_path = continuation_record_path(
                    root,
                    &self.parent_session_id,
                    &record.continuation_id,
                );
                match fs::symlink_metadata(&record_path) {
                    Ok(metadata) if metadata.is_file() => {
                        let existing =
                            read_record(root, &self.parent_session_id, &record.continuation_id)?;
                        if existing != record {
                            return Err(corrupt_record(
                                "unindexed continuation record conflicts with requested creation",
                            ));
                        }
                        index.insert(
                            record.continuation_id.as_str().to_string(),
                            self.parent_session_id.clone(),
                        );
                        write_json_pretty(
                            &continuation_index_path(root),
                            &index,
                            "write continuation index",
                        )?;
                        cache_record(cache, existing.clone())?;
                        return Ok(existing);
                    }
                    Ok(_) => {
                        return Err(corrupt_record(
                            "unindexed continuation path is not a regular file",
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(persistence_io_error(
                            "inspect continuation record path",
                            &error,
                        ));
                    }
                }

                write_json_pretty(&record_path, &record, "write continuation record")?;
                index.insert(
                    record.continuation_id.as_str().to_string(),
                    self.parent_session_id.clone(),
                );
                write_json_pretty(
                    &continuation_index_path(root),
                    &index,
                    "write continuation index",
                )?;
                cache_record(cache, record.clone())?;
                Ok(record)
            }
        }
    }

    /// Loads one continuation strictly through the global index without directory guessing.
    pub(crate) fn load_record(
        &self,
        continuation_id: &AgentContinuationId,
    ) -> Result<Option<AgentContinuationRecord>, AgentContinuationError> {
        match &self.backend {
            AgentContinuationStoreBackend::ProcessLocal { state } => {
                let state = lock_process_state(state)?;
                let Some(parent_session_id) = state.index.get(continuation_id) else {
                    return Ok(None);
                };
                ensure_parent_matches(&self.parent_session_id, parent_session_id)?;
                let record = state.records.get(continuation_id).cloned().ok_or_else(|| {
                    corrupt_record("continuation index points to a missing record")
                })?;
                validate_record(&record, Some(continuation_id), Some(parent_session_id))?;
                Ok(Some(record))
            }
            AgentContinuationStoreBackend::Persistent { root, cache } => {
                let Some(parent_session_id) = locate_indexed_parent(root, continuation_id)? else {
                    return Ok(None);
                };
                ensure_parent_matches(&self.parent_session_id, &parent_session_id)?;
                let _session_lock = acquire_lock(
                    &session_lock_path(root, &parent_session_id),
                    "acquire continuation session lock",
                )?;
                ensure_index_mapping(root, continuation_id, &parent_session_id)?;
                let record = read_record(root, &parent_session_id, continuation_id)?;
                cache_record(cache, record.clone())?;
                Ok(Some(record))
            }
        }
    }

    /// Mutates one current record under its session lock and advances its CAS revision once.
    pub(crate) fn mutate_record<R, F>(
        &self,
        continuation_id: &AgentContinuationId,
        expected_revision: ContinuationRevision,
        mutate: F,
    ) -> Result<(R, AgentContinuationRecord), AgentContinuationError>
    where
        F: FnOnce(&mut AgentContinuationRecord) -> Result<R, AgentContinuationError>,
    {
        match &self.backend {
            AgentContinuationStoreBackend::ProcessLocal { state } => {
                let mut state = lock_process_state(state)?;
                let parent_session_id = state
                    .index
                    .get(continuation_id)
                    .cloned()
                    .ok_or_else(|| corrupt_record("continuation index entry is missing"))?;
                ensure_parent_matches(&self.parent_session_id, &parent_session_id)?;
                let current = state.records.get(continuation_id).cloned().ok_or_else(|| {
                    corrupt_record("continuation index points to a missing record")
                })?;
                let (result, committed) = mutate_validated_record(
                    current,
                    continuation_id,
                    &parent_session_id,
                    expected_revision,
                    mutate,
                )?;
                state
                    .records
                    .insert(continuation_id.clone(), committed.clone());
                Ok((result, committed))
            }
            AgentContinuationStoreBackend::Persistent { root, cache } => {
                let parent_session_id = locate_indexed_parent(root, continuation_id)?
                    .ok_or_else(|| corrupt_record("continuation index entry is missing"))?;
                ensure_parent_matches(&self.parent_session_id, &parent_session_id)?;
                let _session_lock = acquire_lock(
                    &session_lock_path(root, &parent_session_id),
                    "acquire continuation session lock",
                )?;
                ensure_index_mapping(root, continuation_id, &parent_session_id)?;
                let current = read_record(root, &parent_session_id, continuation_id)?;
                let (result, committed) = mutate_validated_record(
                    current,
                    continuation_id,
                    &parent_session_id,
                    expected_revision,
                    mutate,
                )?;
                write_json_pretty(
                    &continuation_record_path(root, &parent_session_id, continuation_id),
                    &committed,
                    "write continuation record",
                )?;
                cache_record(cache, committed.clone())?;
                Ok((result, committed))
            }
        }
    }

    /// Lists all valid records indexed to this store's parent session.
    pub(crate) fn list_parent_records(
        &self,
    ) -> Result<Vec<AgentContinuationRecord>, AgentContinuationError> {
        match &self.backend {
            AgentContinuationStoreBackend::ProcessLocal { state } => {
                let state = lock_process_state(state)?;
                let mut records = Vec::new();
                for (continuation_id, parent_session_id) in &state.index {
                    if parent_session_id != &self.parent_session_id {
                        continue;
                    }
                    let record = state.records.get(continuation_id).cloned().ok_or_else(|| {
                        corrupt_record("continuation index points to a missing record")
                    })?;
                    validate_record(&record, Some(continuation_id), Some(parent_session_id))?;
                    records.push(record);
                }
                sort_records(&mut records);
                Ok(records)
            }
            AgentContinuationStoreBackend::Persistent { root, cache } => {
                let _session_lock = acquire_lock(
                    &session_lock_path(root, &self.parent_session_id),
                    "acquire continuation session lock",
                )?;
                let records = load_parent_records_unlocked(root, &self.parent_session_id)?;
                cache_records(cache, &records)?;
                Ok(records)
            }
        }
    }

    /// Reconciles expired execution owners before legacy task recovery runs.
    /// A safe checkpoint becomes Suspended; an attempt without a trustworthy
    /// checkpoint becomes Indeterminate so external work is never replayed.
    pub(crate) fn reconcile_expired_owners(
        &self,
    ) -> Result<Vec<ContinuationProjection>, AgentContinuationError> {
        self.reconcile_expired_owners_locked()
            .map(ContinuationReconciliation::into_projections)
    }

    /// Reconciles expired owners while retaining the persistent session lock.
    /// Callers may apply the returned projections to task records before this
    /// guard is dropped, keeping recovery on one authoritative snapshot.
    pub(crate) fn reconcile_expired_owners_locked(
        &self,
    ) -> Result<ContinuationReconciliation, AgentContinuationError> {
        let now_ms = continuation_now_ms();
        match &self.backend {
            AgentContinuationStoreBackend::ProcessLocal { state } => {
                let mut state = lock_process_state(state)?;
                let mut records = state
                    .index
                    .iter()
                    .filter(|(_, parent_session_id)| *parent_session_id == &self.parent_session_id)
                    .map(|(continuation_id, parent_session_id)| {
                        let record =
                            state.records.get(continuation_id).cloned().ok_or_else(|| {
                                corrupt_record("continuation index points to a missing record")
                            })?;
                        validate_record(&record, Some(continuation_id), Some(parent_session_id))?;
                        Ok(record)
                    })
                    .collect::<Result<Vec<_>, AgentContinuationError>>()?;
                sort_records(&mut records);
                let mut projections = Vec::with_capacity(records.len());
                for record in records {
                    let (record, changed) = reconcile_expired_record(record, now_ms)?;
                    if changed {
                        state
                            .records
                            .insert(record.continuation_id.clone(), record.clone());
                    }
                    projections.push(ContinuationProjection::from(&record));
                }
                Ok(ContinuationReconciliation {
                    projections,
                    _session_lock: None,
                })
            }
            AgentContinuationStoreBackend::Persistent { root, cache } => {
                let session_lock = acquire_lock(
                    &session_lock_path(root, &self.parent_session_id),
                    "acquire continuation session lock",
                )?;
                let records = load_parent_records_unlocked(root, &self.parent_session_id)?;
                let mut reconciled_records = Vec::with_capacity(records.len());
                for record in records {
                    let (record, changed) = reconcile_expired_record(record, now_ms)?;
                    if changed {
                        write_json_pretty(
                            &continuation_record_path(
                                root,
                                &self.parent_session_id,
                                &record.continuation_id,
                            ),
                            &record,
                            "write continuation record",
                        )?;
                    }
                    reconciled_records.push(record);
                }
                cache_records(cache, &reconciled_records)?;
                let projections = reconciled_records
                    .iter()
                    .map(ContinuationProjection::from)
                    .collect();
                Ok(ContinuationReconciliation {
                    projections,
                    _session_lock: Some(session_lock),
                })
            }
        }
    }
}

fn load_parent_records_unlocked(
    root: &Path,
    parent_session_id: &str,
) -> Result<Vec<AgentContinuationRecord>, AgentContinuationError> {
    let index = load_continuation_index(root)?;
    let mut continuation_ids = index
        .iter()
        .filter(|(_, indexed_parent_session_id)| *indexed_parent_session_id == parent_session_id)
        .map(|(continuation_id, _)| {
            AgentContinuationId::parse(continuation_id.clone())
                .map_err(|_| corrupt_record("continuation index contains an invalid identity"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    continuation_ids.sort();
    continuation_ids
        .into_iter()
        .map(|continuation_id| read_record(root, parent_session_id, &continuation_id))
        .collect()
}

fn reconcile_expired_record(
    record: AgentContinuationRecord,
    now_ms: i64,
) -> Result<(AgentContinuationRecord, bool), AgentContinuationError> {
    let expired_owner = record.current_attempt.state == AttemptState::Running
        && record.current_attempt.owner_id.is_some()
        && record
            .current_attempt
            .lease_expires_at_ms
            .is_none_or(|expires_at_ms| expires_at_ms <= now_ms);
    if !expired_owner {
        return Ok((record, false));
    }
    let continuation_id = record.continuation_id.clone();
    let parent_session_id = record.parent_session_id.clone();
    let expected_revision = record.revision;
    let (_, reconciled) = mutate_validated_record(
        record,
        &continuation_id,
        &parent_session_id,
        expected_revision,
        move |record| {
            if record.current_attempt.state != AttemptState::Running
                || record_has_live_owner(record, now_ms)
            {
                return Ok(());
            }
            record.current_attempt.owner_id = None;
            record.current_attempt.lease_expires_at_ms = None;
            record.updated_at_ms = now_ms;
            if record.checkpoint.is_some()
                && record.status == ContinuationStatus::Checkpointed
                && !record
                    .active_tool_boundary
                    .as_ref()
                    .is_some_and(|boundary| matches!(boundary, ToolBoundary::Indeterminate { .. }))
            {
                record.status.transition_to(ContinuationStatus::Suspended)?;
                record
                    .current_attempt
                    .state
                    .transition_to(AttemptState::Suspended)?;
            } else {
                record
                    .status
                    .transition_to(ContinuationStatus::Indeterminate)?;
                record
                    .current_attempt
                    .state
                    .transition_to(AttemptState::Terminal)?;
                record.current_attempt.completed_at_ms = Some(now_ms);
                record.active_tool_boundary = None;
                record.terminal = Some(AgentTerminal::Indeterminate {
                    reason: "continuation owner expired without a safe checkpoint; inspect external state before retrying"
                        .to_string(),
                });
            }
            Ok(())
        },
    )?;
    Ok((reconciled, true))
}

fn lock_process_state(
    state: &Arc<Mutex<ProcessLocalContinuationState>>,
) -> Result<std::sync::MutexGuard<'_, ProcessLocalContinuationState>, AgentContinuationError> {
    state
        .lock()
        .map_err(|_| persistence_error("continuation process-local state lock is poisoned"))
}

fn cache_record(
    cache: &Arc<Mutex<HashMap<AgentContinuationId, AgentContinuationRecord>>>,
    record: AgentContinuationRecord,
) -> Result<(), AgentContinuationError> {
    cache
        .lock()
        .map_err(|_| persistence_error("continuation process cache lock is poisoned"))?
        .insert(record.continuation_id.clone(), record);
    Ok(())
}

fn cache_records(
    cache: &Arc<Mutex<HashMap<AgentContinuationId, AgentContinuationRecord>>>,
    records: &[AgentContinuationRecord],
) -> Result<(), AgentContinuationError> {
    let mut cache = cache
        .lock()
        .map_err(|_| persistence_error("continuation process cache lock is poisoned"))?;
    for record in records {
        cache.insert(record.continuation_id.clone(), record.clone());
    }
    Ok(())
}

fn mutate_validated_record<R, F>(
    mut record: AgentContinuationRecord,
    continuation_id: &AgentContinuationId,
    parent_session_id: &str,
    expected_revision: ContinuationRevision,
    mutate: F,
) -> Result<(R, AgentContinuationRecord), AgentContinuationError>
where
    F: FnOnce(&mut AgentContinuationRecord) -> Result<R, AgentContinuationError>,
{
    validate_record(&record, Some(continuation_id), Some(parent_session_id))?;
    if record.revision != expected_revision {
        return Err(AgentContinuationError::RevisionConflict {
            expected: expected_revision,
            actual: record.revision,
        });
    }
    let previous_updated_at_ms = record.updated_at_ms;
    let result = mutate(&mut record)?;
    if record.revision != expected_revision {
        return Err(corrupt_record(
            "continuation mutation must not modify the store-owned revision",
        ));
    }
    if record.updated_at_ms < previous_updated_at_ms {
        return Err(corrupt_record(
            "continuation mutation moved the update time backwards",
        ));
    }
    record.revision = expected_revision.next()?;
    validate_record(&record, Some(continuation_id), Some(parent_session_id))?;
    Ok((result, record))
}

fn validate_record(
    record: &AgentContinuationRecord,
    expected_continuation_id: Option<&AgentContinuationId>,
    expected_parent_session_id: Option<&str>,
) -> Result<(), AgentContinuationError> {
    if record.schema_version != AGENT_CONTINUATION_SCHEMA_VERSION {
        return Err(AgentContinuationError::UnsupportedSchemaVersion {
            found: record.schema_version,
        });
    }
    if let Some(expected) = expected_continuation_id
        && expected != &record.continuation_id
    {
        return Err(AgentContinuationError::ContinuationMismatch {
            expected: expected.clone(),
            actual: record.continuation_id.clone(),
        });
    }
    if let Some(expected) = expected_parent_session_id {
        ensure_parent_matches(expected, &record.parent_session_id)?;
    }
    if record.parent_session_id.is_empty()
        || record.source_task_id.is_empty()
        || record.latest_task_id.is_empty()
        || record.subagent_type.is_empty()
        || record.effective_cwd.is_empty()
        || record
            .parent_task_id
            .as_ref()
            .is_some_and(|task_id| task_id.is_empty())
    {
        return Err(corrupt_record(
            "continuation record contains an empty required binding",
        ));
    }
    if record.created_at_ms > record.updated_at_ms {
        return Err(corrupt_record(
            "continuation update time predates creation time",
        ));
    }
    if record.current_attempt.resumed_from_attempt_id.as_ref()
        == Some(&record.current_attempt.attempt_id)
    {
        return Err(corrupt_record("continuation attempt resumes from itself"));
    }
    if record.current_attempt.completed_at_ms.is_some()
        != (record.current_attempt.state == AttemptState::Terminal)
    {
        return Err(corrupt_record(
            "continuation attempt completion does not match its state",
        ));
    }
    if record.current_attempt.owner_id.is_some()
        != record.current_attempt.lease_expires_at_ms.is_some()
    {
        return Err(corrupt_record(
            "continuation lease owner and expiry must be present together",
        ));
    }
    match record.current_attempt.state {
        AttemptState::Prepared | AttemptState::Terminal
            if record.current_attempt.owner_id.is_some() =>
        {
            return Err(corrupt_record(
                "inactive continuation attempt retains a lease owner",
            ));
        }
        AttemptState::Running if record.current_attempt.owner_id.is_none() => {
            return Err(corrupt_record(
                "running continuation attempt has no lease owner",
            ));
        }
        _ => {}
    }
    if matches!(
        record.current_attempt.state,
        AttemptState::Running | AttemptState::Suspended
    ) && record.current_attempt.started_at_ms.is_none()
    {
        return Err(corrupt_record(
            "active continuation attempt has no start time",
        ));
    }
    if let (Some(started_at_ms), Some(completed_at_ms)) = (
        record.current_attempt.started_at_ms,
        record.current_attempt.completed_at_ms,
    ) && completed_at_ms < started_at_ms
    {
        return Err(corrupt_record(
            "continuation attempt completes before it starts",
        ));
    }
    match record.isolation {
        SubagentIsolation::None if record.worktree.is_some() => {
            return Err(corrupt_record(
                "non-worktree continuation has a worktree binding",
            ));
        }
        SubagentIsolation::Worktree if record.worktree.is_none() => {
            return Err(corrupt_record(
                "worktree continuation is missing its binding",
            ));
        }
        _ => {}
    }

    validate_status_structure(record)?;
    if let Some(checkpoint) = &record.checkpoint {
        if checkpoint.conversation.schema_version != CHILD_CONVERSATION_SNAPSHOT_SCHEMA_VERSION {
            return Err(AgentContinuationError::UnsupportedSchemaVersion {
                found: checkpoint.conversation.schema_version,
            });
        }
        let checkpoint_matches_current = checkpoint.attempt_id == record.current_attempt.attempt_id;
        let checkpoint_matches_resumed_from =
            record.current_attempt.resumed_from_attempt_id.as_ref() == Some(&checkpoint.attempt_id);
        if !checkpoint_matches_current && !checkpoint_matches_resumed_from {
            return Err(AgentContinuationError::AttemptMismatch {
                expected: record.current_attempt.attempt_id.clone(),
                actual: checkpoint.attempt_id.clone(),
            });
        }
        checkpoint.verify_digest()?;
    }
    Ok(())
}

fn validate_status_structure(
    record: &AgentContinuationRecord,
) -> Result<(), AgentContinuationError> {
    let attempt_state_valid = match record.status {
        ContinuationStatus::Created | ContinuationStatus::Resuming => {
            record.current_attempt.state == AttemptState::Prepared
        }
        ContinuationStatus::Running | ContinuationStatus::Checkpointed => {
            record.current_attempt.state == AttemptState::Running
        }
        ContinuationStatus::Suspended => record.current_attempt.state == AttemptState::Suspended,
        ContinuationStatus::Completed
        | ContinuationStatus::Failed
        | ContinuationStatus::Cancelled
        | ContinuationStatus::Indeterminate => {
            record.current_attempt.state == AttemptState::Terminal
        }
    };
    if !attempt_state_valid {
        return Err(corrupt_record(
            "continuation status does not match current attempt state",
        ));
    }

    let terminal_valid = matches!(
        (record.status, record.terminal.as_ref()),
        (
            ContinuationStatus::Created
                | ContinuationStatus::Running
                | ContinuationStatus::Checkpointed
                | ContinuationStatus::Suspended
                | ContinuationStatus::Resuming,
            None
        ) | (
            ContinuationStatus::Completed,
            Some(AgentTerminal::Completed { .. })
        ) | (
            ContinuationStatus::Failed,
            Some(AgentTerminal::Failed { .. })
        ) | (
            ContinuationStatus::Cancelled,
            Some(AgentTerminal::Cancelled { .. })
        ) | (
            ContinuationStatus::Indeterminate,
            Some(AgentTerminal::Indeterminate { .. })
        )
    );
    if !terminal_valid {
        return Err(corrupt_record(
            "continuation terminal outcome does not match status",
        ));
    }
    if matches!(
        record.status,
        ContinuationStatus::Checkpointed
            | ContinuationStatus::Suspended
            | ContinuationStatus::Resuming
    ) && record.checkpoint.is_none()
    {
        return Err(corrupt_record(
            "continuation status requires a safe checkpoint",
        ));
    }
    if record.status == ContinuationStatus::Created && record.checkpoint.is_some() {
        return Err(corrupt_record(
            "created continuation must not contain a checkpoint",
        ));
    }
    if record.current_attempt.state == AttemptState::Terminal
        && record.active_tool_boundary.is_some()
    {
        return Err(corrupt_record(
            "terminal continuation retains an active tool boundary",
        ));
    }
    Ok(())
}

fn locate_indexed_parent(
    root: &Path,
    continuation_id: &AgentContinuationId,
) -> Result<Option<String>, AgentContinuationError> {
    Ok(load_continuation_index(root)?
        .get(continuation_id.as_str())
        .cloned())
}

fn ensure_index_mapping(
    root: &Path,
    continuation_id: &AgentContinuationId,
    expected_parent_session_id: &str,
) -> Result<(), AgentContinuationError> {
    let index = load_continuation_index(root)?;
    let actual_parent_session_id = index
        .get(continuation_id.as_str())
        .ok_or_else(|| corrupt_record("continuation index entry disappeared"))?;
    ensure_parent_matches(expected_parent_session_id, actual_parent_session_id)
}

fn ensure_parent_matches(
    expected_parent_session_id: &str,
    actual_parent_session_id: &str,
) -> Result<(), AgentContinuationError> {
    if expected_parent_session_id != actual_parent_session_id {
        return Err(AgentContinuationError::ParentMismatch {
            expected: expected_parent_session_id.to_string(),
            actual: actual_parent_session_id.to_string(),
        });
    }
    Ok(())
}

fn load_continuation_index(root: &Path) -> Result<HashMap<String, String>, AgentContinuationError> {
    let bytes = match fs::read(continuation_index_path(root)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(persistence_io_error("read continuation index", &error)),
    };
    let index: HashMap<String, String> = serde_json::from_slice(&bytes)
        .map_err(|_| corrupt_record("continuation index is not valid JSON"))?;
    for (continuation_id, parent_session_id) in &index {
        if parent_session_id.is_empty()
            || AgentContinuationId::parse(continuation_id.clone()).is_err()
        {
            return Err(corrupt_record(
                "continuation index contains an invalid entry",
            ));
        }
    }
    Ok(index)
}

fn read_record(
    root: &Path,
    parent_session_id: &str,
    continuation_id: &AgentContinuationId,
) -> Result<AgentContinuationRecord, AgentContinuationError> {
    let path = continuation_record_path(root, parent_session_id, continuation_id);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(corrupt_record(
                "continuation index points to a missing record",
            ));
        }
        Err(error) => return Err(persistence_io_error("read continuation record", &error)),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| corrupt_record("continuation record is not valid JSON"))?;
    let schema_version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| corrupt_record("continuation record has no valid schema version"))?;
    if schema_version != AGENT_CONTINUATION_SCHEMA_VERSION {
        return Err(AgentContinuationError::UnsupportedSchemaVersion {
            found: schema_version,
        });
    }
    let record: AgentContinuationRecord = serde_json::from_value(value)
        .map_err(|_| corrupt_record("continuation record does not match its schema"))?;
    validate_record(&record, Some(continuation_id), Some(parent_session_id))?;
    Ok(record)
}

fn write_json_pretty<T: Serialize>(
    path: &Path,
    value: &T,
    operation: &'static str,
) -> Result<(), AgentContinuationError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| persistence_io_error("create continuation directory", &error))?;
    }
    let content =
        serde_json::to_vec_pretty(value).map_err(|_| AgentContinuationError::Persistence {
            message: format!("failed to serialize data for {operation}"),
        })?;
    atomic_write(path, &content, AtomicWritePolicy::NoFollow)
        .map_err(|_| persistence_error(operation))
}

fn acquire_lock(
    path: &Path,
    operation: &'static str,
) -> Result<ExclusiveFileLock, AgentContinuationError> {
    ExclusiveFileLock::acquire(path).map_err(|_| persistence_error(operation))
}

fn continuation_index_path(root: &Path) -> PathBuf {
    root.join("continuation-index.json")
}

fn continuation_index_lock_path(root: &Path) -> PathBuf {
    root.join("continuation-index.lock")
}

fn session_lock_path(root: &Path, parent_session_id: &str) -> PathBuf {
    root.join(safe_path_component(parent_session_id))
        .join("tasks.lock")
}

fn continuation_record_path(
    root: &Path,
    parent_session_id: &str,
    continuation_id: &AgentContinuationId,
) -> PathBuf {
    root.join(safe_path_component(parent_session_id))
        .join("continuations")
        .join(format!("{}.json", continuation_id.as_str()))
}

fn sort_records(records: &mut [AgentContinuationRecord]) {
    records.sort_by(|left, right| left.continuation_id.cmp(&right.continuation_id));
}

fn corrupt_record(message: &'static str) -> AgentContinuationError {
    AgentContinuationError::CorruptRecord {
        message: message.to_string(),
    }
}

fn persistence_error(message: &'static str) -> AgentContinuationError {
    AgentContinuationError::Persistence {
        message: message.to_string(),
    }
}

fn persistence_io_error(operation: &'static str, error: &io::Error) -> AgentContinuationError {
    AgentContinuationError::Persistence {
        message: format!("failed to {operation} ({:?})", error.kind()),
    }
}

/// Authority-bearing lease returned after a prepared attempt is acquired.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ContinuationLease {
    /// Continuation lineage protected by this lease.
    pub(crate) continuation_id: AgentContinuationId,
    /// Attempt protected by this lease.
    pub(crate) attempt_id: AgentAttemptId,
    /// Runtime owner that acquired the lease.
    pub(crate) owner_id: String,
    /// Monotonic lease fencing epoch.
    pub(crate) lease_epoch: u64,
    /// Diagnostic lease expiry in Unix milliseconds.
    pub(crate) expires_at_ms: i64,
    /// Revision committed by acquisition and expected by the first write.
    pub(crate) revision: ContinuationRevision,
}

impl ContinuationLease {
    /// Constructs the full identity, epoch, and revision fence for a commit.
    pub(crate) fn fence(&self, expected_revision: ContinuationRevision) -> ContinuationFence {
        ContinuationFence {
            continuation_id: self.continuation_id.clone(),
            attempt_id: self.attempt_id.clone(),
            lease_epoch: self.lease_epoch,
            expected_revision,
        }
    }
}

/// Compound stale-writer fence required by every continuation mutation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ContinuationFence {
    /// Expected continuation lineage identity.
    pub(crate) continuation_id: AgentContinuationId,
    /// Expected current attempt identity.
    pub(crate) attempt_id: AgentAttemptId,
    /// Expected current lease epoch.
    pub(crate) lease_epoch: u64,
    /// Expected continuation revision.
    pub(crate) expected_revision: ContinuationRevision,
}

impl ContinuationFence {
    /// Validates all identity, lease epoch, and revision components against a record.
    pub(crate) fn validate(
        &self,
        record: &AgentContinuationRecord,
    ) -> Result<(), AgentContinuationError> {
        if self.continuation_id != record.continuation_id {
            return Err(AgentContinuationError::ContinuationMismatch {
                expected: self.continuation_id.clone(),
                actual: record.continuation_id.clone(),
            });
        }
        if self.attempt_id != record.current_attempt.attempt_id {
            return Err(AgentContinuationError::AttemptMismatch {
                expected: self.attempt_id.clone(),
                actual: record.current_attempt.attempt_id.clone(),
            });
        }
        if self.lease_epoch != record.current_attempt.lease_epoch {
            return Err(AgentContinuationError::LeaseEpochMismatch {
                expected: self.lease_epoch,
                actual: record.current_attempt.lease_epoch,
            });
        }
        if self.expected_revision != record.revision {
            return Err(AgentContinuationError::RevisionConflict {
                expected: self.expected_revision,
                actual: record.revision,
            });
        }
        Ok(())
    }
}

/// Read-only continuation state shared by task and surface projections.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ContinuationProjection {
    /// Stable continuation lineage identity.
    pub(crate) continuation_id: AgentContinuationId,
    /// Current attempt identity.
    pub(crate) attempt_id: AgentAttemptId,
    /// Latest safe checkpoint identity, if one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checkpoint_id: Option<AgentCheckpointId>,
    /// Current continuation revision.
    pub(crate) revision: ContinuationRevision,
    /// Authoritative continuation lifecycle state.
    pub(crate) status: ContinuationStatus,
    /// Current attempt lifecycle state.
    pub(crate) attempt_state: AttemptState,
    /// Task associated with the current attempt.
    pub(crate) latest_task_id: String,
    /// Latest checkpoint sequence, if one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checkpoint_sequence: Option<u64>,
    /// Completed turn count represented by the latest checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn: Option<u32>,
    /// Whether an explicit resume may safely create a new attempt.
    #[serde(default)]
    pub(crate) resumable: bool,
    /// Whether unknown external side effects block automatic replay.
    #[serde(default)]
    pub(crate) indeterminate: bool,
    /// Current lease epoch used for stale-writer fencing.
    #[serde(default)]
    pub(crate) lease_epoch: u64,
    /// Diagnostic lease expiry in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lease_expires_at_ms: Option<i64>,
    /// Last durable update time in Unix milliseconds.
    pub(crate) updated_at_ms: i64,
}

impl From<&AgentContinuationRecord> for ContinuationProjection {
    fn from(record: &AgentContinuationRecord) -> Self {
        let indeterminate = record_is_indeterminate(record);
        Self {
            continuation_id: record.continuation_id.clone(),
            attempt_id: record.current_attempt.attempt_id.clone(),
            checkpoint_id: record
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.checkpoint_id.clone()),
            revision: record.revision,
            status: record.status,
            attempt_state: record.current_attempt.state,
            latest_task_id: record.latest_task_id.clone(),
            checkpoint_sequence: record
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.sequence),
            turn: record.checkpoint.as_ref().map(|checkpoint| checkpoint.turn),
            resumable: !indeterminate
                && record.checkpoint.is_some()
                && matches!(
                    record.status,
                    ContinuationStatus::Checkpointed
                        | ContinuationStatus::Suspended
                        | ContinuationStatus::Completed
                        | ContinuationStatus::Failed
                        | ContinuationStatus::Cancelled
                )
                && !record_has_live_owner(record, continuation_now_ms()),
            indeterminate,
            lease_epoch: record.current_attempt.lease_epoch,
            lease_expires_at_ms: record.current_attempt.lease_expires_at_ms,
            updated_at_ms: record.updated_at_ms,
        }
    }
}

/// Runtime-owned coordinator for child continuation creation, resumption, and settlement.
#[derive(Clone)]
pub(crate) struct ChildAgentCoordinator {
    task_registry: TaskRegistry,
    store: AgentContinuationStore,
    owner_id: Arc<str>,
}

impl ChildAgentCoordinator {
    /// Creates a cloneable coordinator with one stable runtime owner identity; it returns persistence errors from the registry store lookup and otherwise changes no durable state.
    pub(crate) fn new(task_registry: TaskRegistry) -> Result<Self, AgentContinuationError> {
        let owner_id = format!(
            "continuation:{}:{}",
            std::process::id(),
            uuid::Uuid::now_v7().hyphenated()
        );
        Self::with_owner_id(task_registry, owner_id)
    }

    /// Creates a cloneable coordinator with an explicit non-empty owner identity; it returns validation or persistence errors and otherwise changes no durable state.
    pub(crate) fn with_owner_id(
        task_registry: TaskRegistry,
        owner_id: String,
    ) -> Result<Self, AgentContinuationError> {
        if owner_id.trim().is_empty() {
            return Err(corrupt_record("continuation owner id is empty"));
        }
        let store = task_registry.continuation_store().map_err(|message| {
            AgentContinuationError::Persistence {
                message: format!("failed to open continuation store: {message}"),
            }
        })?;
        Ok(Self {
            task_registry,
            store,
            owner_id: Arc::from(owner_id),
        })
    }

    /// Returns the stable owner identity shared by every clone of this coordinator without changing state.
    pub(crate) fn owner_id(&self) -> &str {
        &self.owner_id
    }

    /// Creates a revision-zero Created/Prepared lineage and installs its task projection; it rejects invalid bindings or duplicate identities, persists the record first, and reports projection failure without rolling the record back.
    pub(crate) fn create(
        &self,
        input: CreateContinuationInput,
    ) -> Result<PreparedContinuation, AgentContinuationError> {
        validate_task_binding(&input.parent_task_id, &input.task_id)?;
        validate_compatibility_shape(&input.compatibility)?;
        let continuation_id = input
            .continuation_id
            .unwrap_or_else(AgentContinuationId::new);
        if let Some(record) = self.store.load_record(&continuation_id)? {
            let duplicate = record.parent_task_id == input.parent_task_id
                && record.source_task_id == input.task_id
                && record.latest_task_id == input.task_id
                && record.current_attempt.prompt_id == input.prompt_id
                && record.subagent_type == input.compatibility.subagent_type
                && record.model == input.compatibility.model
                && record.isolation == input.compatibility.isolation
                && record.effective_cwd == input.compatibility.effective_cwd
                && record.worktree == input.compatibility.worktree
                && record.compatibility_hash == input.compatibility.compatibility_hash;
            if duplicate {
                self.install_projection(&record)?;
                return Ok(PreparedContinuation::from(&record));
            }
            return Err(AgentContinuationError::AlreadyExists { continuation_id });
        }

        let now_ms = continuation_now_ms();
        let record = AgentContinuationRecord {
            schema_version: AGENT_CONTINUATION_SCHEMA_VERSION,
            continuation_id,
            parent_session_id: self.store.parent_session_id().to_string(),
            parent_task_id: input.parent_task_id,
            source_task_id: input.task_id.clone(),
            latest_task_id: input.task_id,
            subagent_type: input.compatibility.subagent_type,
            model: input.compatibility.model,
            isolation: input.compatibility.isolation,
            effective_cwd: input.compatibility.effective_cwd,
            worktree: input.compatibility.worktree,
            compatibility_hash: input.compatibility.compatibility_hash,
            revision: ContinuationRevision::ZERO,
            status: ContinuationStatus::Created,
            current_attempt: AgentAttemptRecord {
                attempt_id: AgentAttemptId::new(),
                resumed_from_attempt_id: None,
                owner_id: None,
                lease_epoch: 0,
                lease_expires_at_ms: None,
                state: AttemptState::Prepared,
                prompt_id: input.prompt_id,
                started_at_ms: None,
                completed_at_ms: None,
            },
            checkpoint: None,
            active_tool_boundary: None,
            terminal: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let record = self.store.create_record(record)?;
        self.install_projection(&record)?;
        Ok(PreparedContinuation::from(&record))
    }

    /// Resolves a UUIDv7 continuation directly or a compatibility task/agent selector through the task registry; it returns stable not-found or persistence errors and performs no writes.
    pub(crate) fn resolve_selector(
        &self,
        selector: &str,
    ) -> Result<AgentContinuationId, AgentContinuationError> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err(AgentContinuationError::NotFound);
        }
        if let Ok(continuation_id) = AgentContinuationId::parse(selector.to_string()) {
            return Ok(continuation_id);
        }
        self.task_registry
            .continuation_projection(selector)
            .map_err(|message| AgentContinuationError::Persistence {
                message: format!("failed to resolve continuation task projection: {message}"),
            })?
            .map(|projection| projection.continuation_id)
            .ok_or(AgentContinuationError::NotFound)
    }

    /// Loads the authoritative prepared/source facts for a UUIDv7
    /// continuation or task/agent selector. It validates durable compatibility
    /// and checkpoint integrity, returns stable lookup or corruption errors,
    /// and performs no writes.
    pub(crate) fn prepared(
        &self,
        selector: &str,
    ) -> Result<PreparedContinuation, AgentContinuationError> {
        let continuation_id = self.resolve_selector(selector)?;
        let record = self
            .store
            .load_record(&continuation_id)?
            .ok_or(AgentContinuationError::NotFound)?;
        let prepared = PreparedContinuation::from(&record);
        validate_compatibility_shape(&prepared.compatibility)?;
        if let Some(checkpoint) = prepared.checkpoint.as_ref() {
            checkpoint.verify_digest()?;
            ensure_checkpoint_boundary_safe(checkpoint)?;
        }
        Ok(prepared)
    }

    /// Validates parent, compatibility, workspace, checkpoint, owner, and prompt idempotency before preparing one new Resuming/Prepared attempt; it persists the new attempt and then installs its task projection without rolling back a committed record on projection failure.
    pub(crate) fn prepare_resume(
        &self,
        input: ResumeContinuationInput,
    ) -> Result<PreparedContinuation, AgentContinuationError> {
        validate_task_binding(&input.parent_task_id, &input.task_id)?;
        validate_compatibility_shape(&input.compatibility)?;
        let continuation_id = self.resolve_selector(&input.selector)?;
        let record = self
            .store
            .load_record(&continuation_id)?
            .ok_or(AgentContinuationError::NotFound)?;
        validate_resume_request(&record, &input)?;

        if record.current_attempt.prompt_id == input.prompt_id {
            self.install_projection(&record)?;
            return Ok(PreparedContinuation::from(&record));
        }

        let now_ms = continuation_now_ms();
        reject_live_owner(&record, now_ms)?;
        ensure_record_resumable(&record)?;
        let expected_revision = record.revision;
        let prompt_id = input.prompt_id;
        let task_id = input.task_id;
        let compatibility = input.compatibility;
        let parent_task_id = input.parent_task_id;
        let (_, committed) =
            self.store
                .mutate_record(&continuation_id, expected_revision, move |record| {
                    let request = ResumeContinuationInput {
                        selector: record.continuation_id.as_str().to_string(),
                        parent_task_id: parent_task_id.clone(),
                        task_id: task_id.clone(),
                        prompt_id: prompt_id.clone(),
                        compatibility: compatibility.clone(),
                    };
                    validate_resume_request(record, &request)?;
                    reject_live_owner(record, now_ms)?;
                    let checkpoint = ensure_record_resumable(record)?.clone();
                    match record.status {
                        ContinuationStatus::Checkpointed => {
                            record.status.transition_to(ContinuationStatus::Suspended)?;
                            record.status.transition_to(ContinuationStatus::Resuming)?;
                        }
                        ContinuationStatus::Suspended
                        | ContinuationStatus::Completed
                        | ContinuationStatus::Failed
                        | ContinuationStatus::Cancelled => {
                            record.status.transition_to(ContinuationStatus::Resuming)?;
                        }
                        status => return Err(AgentContinuationError::NotResumable { status }),
                    }
                    record.current_attempt = AgentAttemptRecord {
                        attempt_id: AgentAttemptId::new(),
                        resumed_from_attempt_id: Some(checkpoint.attempt_id),
                        owner_id: None,
                        lease_epoch: record.current_attempt.lease_epoch,
                        lease_expires_at_ms: None,
                        state: AttemptState::Prepared,
                        prompt_id,
                        started_at_ms: None,
                        completed_at_ms: None,
                    };
                    record.latest_task_id = task_id;
                    record.active_tool_boundary = None;
                    record.terminal = None;
                    record.updated_at_ms = now_ms;
                    Ok(())
                })?;
        self.install_projection(&committed)?;
        Ok(PreparedContinuation::from(&committed))
    }

    /// Acquires a Prepared attempt for this coordinator owner, or returns its still-valid existing lease on an idempotent retry; it advances status, lease epoch, expiry, revision, and task projection exactly once.
    pub(crate) fn acquire(
        &self,
        prepared: &PreparedContinuation,
    ) -> Result<ContinuationLease, AgentContinuationError> {
        let record = self
            .store
            .load_record(&prepared.continuation_id)?
            .ok_or(AgentContinuationError::NotFound)?;
        validate_prepared_identity(&record, prepared)?;
        let now_ms = continuation_now_ms();
        if record.current_attempt.state == AttemptState::Running {
            let lease = existing_owner_lease(&record, self.owner_id(), now_ms)?;
            self.install_projection(&record)?;
            return Ok(lease);
        }

        let expected_revision = prepared.revision;
        let owner_id = self.owner_id.to_string();
        let expires_at_ms = now_ms
            .checked_add(CONTINUATION_LEASE_DURATION_MS)
            .unwrap_or(i64::MAX);
        let (_, committed) = self.store.mutate_record(
            &prepared.continuation_id,
            expected_revision,
            move |record| {
                validate_prepared_identity(record, prepared)?;
                if record.current_attempt.state != AttemptState::Prepared {
                    return Err(AgentContinuationError::NotResumable {
                        status: record.status,
                    });
                }
                match record.status {
                    ContinuationStatus::Created | ContinuationStatus::Resuming => {
                        record.status.transition_to(ContinuationStatus::Running)?;
                    }
                    status => return Err(AgentContinuationError::NotResumable { status }),
                }
                record
                    .current_attempt
                    .state
                    .transition_to(AttemptState::Running)?;
                record.current_attempt.lease_epoch = record
                    .current_attempt
                    .lease_epoch
                    .checked_add(1)
                    .ok_or_else(|| corrupt_record("continuation lease epoch is exhausted"))?;
                record.current_attempt.owner_id = Some(owner_id);
                record.current_attempt.lease_expires_at_ms = Some(expires_at_ms);
                record.current_attempt.started_at_ms = Some(now_ms);
                record.updated_at_ms = now_ms;
                Ok(())
            },
        )?;
        self.install_projection(&committed)?;
        lease_from_record(&committed)
    }

    /// Settles a prepared attempt that could not start (for example, worker
    /// spawn failure). No lease exists yet, so identity and revision from the
    /// prepared record are the complete fence.
    pub(crate) fn commit_prepared_terminal(
        &self,
        prepared: &PreparedContinuation,
        terminal: AgentTerminal,
    ) -> Result<ContinuationProjection, AgentContinuationError> {
        let now_ms = continuation_now_ms();
        let next_status = terminal_status(&terminal);
        let (_, committed) = self.store.mutate_record(
            &prepared.continuation_id,
            prepared.revision,
            move |record| {
                validate_prepared_identity(record, prepared)?;
                if record.current_attempt.state != AttemptState::Prepared
                    || !matches!(
                        record.status,
                        ContinuationStatus::Created | ContinuationStatus::Resuming
                    )
                {
                    return Err(AgentContinuationError::NotResumable {
                        status: record.status,
                    });
                }
                record.status.transition_to(next_status)?;
                record
                    .current_attempt
                    .state
                    .transition_to(AttemptState::Terminal)?;
                record.current_attempt.completed_at_ms = Some(now_ms);
                record.active_tool_boundary = None;
                record.terminal = Some(terminal);
                record.updated_at_ms = now_ms;
                Ok(())
            },
        )?;
        self.install_projection(&committed)?;
        Ok(ContinuationProjection::from(&committed))
    }

    /// Renews a live attempt lease under the same identity, epoch, and revision
    /// fence used by checkpoint and terminal commits. The returned projection
    /// carries the next revision that the owner must use for its next write.
    pub(crate) fn renew(
        &self,
        lease: &ContinuationLease,
        expected_revision: ContinuationRevision,
    ) -> Result<ContinuationProjection, AgentContinuationError> {
        let now_ms = continuation_now_ms();
        let expires_at_ms = now_ms
            .checked_add(CONTINUATION_LEASE_DURATION_MS)
            .unwrap_or(i64::MAX);
        let fence = lease.fence(expected_revision);
        let (_, committed) =
            self.store
                .mutate_record(&lease.continuation_id, expected_revision, move |record| {
                    fence.validate(record)?;
                    validate_live_lease(record, lease, now_ms)?;
                    if record.current_attempt.state != AttemptState::Running
                        || !matches!(
                            record.status,
                            ContinuationStatus::Running | ContinuationStatus::Checkpointed
                        )
                    {
                        return Err(AgentContinuationError::NotResumable {
                            status: record.status,
                        });
                    }
                    record.current_attempt.lease_expires_at_ms = Some(expires_at_ms);
                    record.updated_at_ms = now_ms;
                    Ok(())
                })?;
        self.install_projection(&committed)?;
        Ok(ContinuationProjection::from(&committed))
    }

    /// Commits one digest-valid monotonically sequenced checkpoint under the full live lease fence; it keeps the attempt Running, advances the record revision, and installs the task projection after durable store success.
    pub(crate) fn commit_checkpoint(
        &self,
        lease: &ContinuationLease,
        expected_revision: ContinuationRevision,
        checkpoint: AgentCheckpoint,
    ) -> Result<ContinuationProjection, AgentContinuationError> {
        checkpoint.verify_digest()?;
        ensure_checkpoint_boundary_safe(&checkpoint)?;
        let current = self
            .store
            .load_record(&lease.continuation_id)?
            .ok_or(AgentContinuationError::NotFound)?;
        if let Some(existing) = current.checkpoint.as_ref()
            && existing.checkpoint_id == checkpoint.checkpoint_id
        {
            if existing != &checkpoint {
                return Err(corrupt_record(
                    "checkpoint retry reuses an identity with different content",
                ));
            }
            if current.current_attempt.attempt_id == lease.attempt_id
                && existing.attempt_id == lease.attempt_id
            {
                self.install_projection(&current)?;
                return Ok(ContinuationProjection::from(&current));
            }
        }
        let now_ms = continuation_now_ms();
        let fence = lease.fence(expected_revision);
        let (_, committed) =
            self.store
                .mutate_record(&lease.continuation_id, expected_revision, move |record| {
                    fence.validate(record)?;
                    validate_live_lease(record, lease, now_ms)?;
                    if checkpoint.attempt_id != record.current_attempt.attempt_id {
                        return Err(AgentContinuationError::AttemptMismatch {
                            expected: record.current_attempt.attempt_id.clone(),
                            actual: checkpoint.attempt_id.clone(),
                        });
                    }
                    let expected_sequence = match record.checkpoint.as_ref() {
                        Some(previous) => previous.sequence.checked_add(1).ok_or_else(|| {
                            corrupt_record("continuation checkpoint sequence is exhausted")
                        })?,
                        None => 0,
                    };
                    if checkpoint.sequence != expected_sequence {
                        return Err(corrupt_record(
                            "continuation checkpoint sequence is not the next value",
                        ));
                    }
                    if record.current_attempt.state != AttemptState::Running
                        || !matches!(
                            record.status,
                            ContinuationStatus::Running | ContinuationStatus::Checkpointed
                        )
                    {
                        return Err(AgentContinuationError::NotResumable {
                            status: record.status,
                        });
                    }
                    if record.status == ContinuationStatus::Running {
                        record
                            .status
                            .transition_to(ContinuationStatus::Checkpointed)?;
                    }
                    record.checkpoint = Some(checkpoint);
                    record.active_tool_boundary = None;
                    record.updated_at_ms = now_ms;
                    Ok(())
                })?;
        self.install_projection(&committed)?;
        Ok(ContinuationProjection::from(&committed))
    }

    /// Commits a terminal outcome under the full live lease fence, returning the existing equal terminal on retry; it settles the attempt, clears ownership, advances revision, and installs the task projection after durable store success.
    pub(crate) fn commit_terminal(
        &self,
        lease: &ContinuationLease,
        expected_revision: ContinuationRevision,
        terminal: AgentTerminal,
    ) -> Result<ContinuationProjection, AgentContinuationError> {
        let current = self
            .store
            .load_record(&lease.continuation_id)?
            .ok_or(AgentContinuationError::NotFound)?;
        if current.current_attempt.attempt_id == lease.attempt_id
            && current.current_attempt.state == AttemptState::Terminal
        {
            if current.terminal.as_ref() != Some(&terminal) {
                return Err(corrupt_record(
                    "terminal retry conflicts with the committed terminal outcome",
                ));
            }
            self.install_projection(&current)?;
            return Ok(ContinuationProjection::from(&current));
        }

        let now_ms = continuation_now_ms();
        let fence = lease.fence(expected_revision);
        let (_, committed) =
            self.store
                .mutate_record(&lease.continuation_id, expected_revision, move |record| {
                    fence.validate(record)?;
                    validate_live_lease(record, lease, now_ms)?;
                    if record.current_attempt.state != AttemptState::Running
                        || !matches!(
                            record.status,
                            ContinuationStatus::Running | ContinuationStatus::Checkpointed
                        )
                    {
                        return Err(AgentContinuationError::NotResumable {
                            status: record.status,
                        });
                    }
                    let next_status = terminal_status(&terminal);
                    record.status.transition_to(next_status)?;
                    record
                        .current_attempt
                        .state
                        .transition_to(AttemptState::Terminal)?;
                    record.current_attempt.owner_id = None;
                    record.current_attempt.lease_expires_at_ms = None;
                    record.current_attempt.completed_at_ms = Some(now_ms);
                    record.active_tool_boundary = None;
                    record.terminal = Some(terminal);
                    record.updated_at_ms = now_ms;
                    Ok(())
                })?;
        self.install_projection(&committed)?;
        Ok(ContinuationProjection::from(&committed))
    }

    /// Persists the replay disposition before a child tool is invoked. This
    /// write shares the continuation lease fence with checkpoint and terminal
    /// commits, so a stale worker cannot weaken an already durable boundary.
    pub(crate) fn commit_tool_boundary(
        &self,
        lease: &ContinuationLease,
        expected_revision: ContinuationRevision,
        boundary: ToolBoundary,
    ) -> Result<ContinuationProjection, AgentContinuationError> {
        let current = self
            .store
            .load_record(&lease.continuation_id)?
            .ok_or(AgentContinuationError::NotFound)?;
        if current.current_attempt.attempt_id == lease.attempt_id
            && current.active_tool_boundary.as_ref() == Some(&boundary)
        {
            self.install_projection(&current)?;
            return Ok(ContinuationProjection::from(&current));
        }
        let now_ms = continuation_now_ms();
        let fence = lease.fence(expected_revision);
        let (_, committed) =
            self.store
                .mutate_record(&lease.continuation_id, expected_revision, move |record| {
                    fence.validate(record)?;
                    validate_live_lease(record, lease, now_ms)?;
                    if record.current_attempt.state != AttemptState::Running {
                        return Err(AgentContinuationError::NotResumable {
                            status: record.status,
                        });
                    }
                    record.active_tool_boundary = Some(boundary);
                    record.updated_at_ms = now_ms;
                    Ok(())
                })?;
        self.install_projection(&committed)?;
        Ok(ContinuationProjection::from(&committed))
    }

    /// Loads the authoritative projection for a continuation UUID or compatibility selector; it returns stable lookup, parent, corruption, or persistence errors and performs no writes.
    pub(crate) fn projection(
        &self,
        selector: &str,
    ) -> Result<ContinuationProjection, AgentContinuationError> {
        let continuation_id = self.resolve_selector(selector)?;
        let record = self
            .store
            .load_record(&continuation_id)?
            .ok_or(AgentContinuationError::NotFound)?;
        Ok(ContinuationProjection::from(&record))
    }

    fn install_projection(
        &self,
        record: &AgentContinuationRecord,
    ) -> Result<(), AgentContinuationError> {
        let projection = ContinuationProjection::from(record);
        self.task_registry
            .install_continuation_projection(&record.latest_task_id, &projection)
            .map_err(|message| AgentContinuationError::Persistence {
                message: format!(
                    "continuation record committed but task projection update failed: {message}"
                ),
            })
    }
}

fn validate_task_binding(
    parent_task_id: &Option<String>,
    task_id: &str,
) -> Result<(), AgentContinuationError> {
    if task_id.trim().is_empty()
        || parent_task_id
            .as_deref()
            .is_some_and(|parent_task_id| parent_task_id.trim().is_empty())
    {
        return Err(corrupt_record(
            "continuation request contains an empty task binding",
        ));
    }
    Ok(())
}

fn validate_compatibility_shape(
    compatibility: &ContinuationCompatibility,
) -> Result<(), AgentContinuationError> {
    if compatibility.subagent_type.trim().is_empty()
        || compatibility.effective_cwd.trim().is_empty()
        || compatibility
            .model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
    {
        return Err(AgentContinuationError::CompatibilityMismatch);
    }
    let cwd = Path::new(&compatibility.effective_cwd);
    if !cwd.is_dir() {
        return Err(AgentContinuationError::CompatibilityMismatch);
    }
    match (compatibility.isolation, compatibility.worktree.as_ref()) {
        (SubagentIsolation::None, None) => Ok(()),
        (SubagentIsolation::Worktree, Some(worktree))
            if !worktree.repo_root.trim().is_empty()
                && !worktree.path.trim().is_empty()
                && Path::new(&worktree.path) == cwd
                && Path::new(&worktree.path).is_dir() =>
        {
            Ok(())
        }
        _ => Err(AgentContinuationError::CompatibilityMismatch),
    }
}

fn validate_resume_request(
    record: &AgentContinuationRecord,
    input: &ResumeContinuationInput,
) -> Result<(), AgentContinuationError> {
    if record.parent_task_id != input.parent_task_id {
        return Err(AgentContinuationError::TaskBindingMismatch {
            expected: task_binding_label(record.parent_task_id.as_deref()),
            actual: task_binding_label(input.parent_task_id.as_deref()),
        });
    }
    let compatibility = &input.compatibility;
    if record.subagent_type != compatibility.subagent_type
        || record.model != compatibility.model
        || record.isolation != compatibility.isolation
        || Path::new(&record.effective_cwd) != Path::new(&compatibility.effective_cwd)
        || record.worktree != compatibility.worktree
        || record.compatibility_hash != compatibility.compatibility_hash
    {
        return Err(AgentContinuationError::CompatibilityMismatch);
    }
    validate_compatibility_shape(compatibility)?;
    if let Some(checkpoint) = &record.checkpoint {
        checkpoint.verify_digest()?;
        ensure_checkpoint_boundary_safe(checkpoint)?;
    }
    if record_is_indeterminate(record) {
        return Err(AgentContinuationError::Indeterminate);
    }
    Ok(())
}

fn validate_prepared_identity(
    record: &AgentContinuationRecord,
    prepared: &PreparedContinuation,
) -> Result<(), AgentContinuationError> {
    if record.continuation_id != prepared.continuation_id {
        return Err(AgentContinuationError::ContinuationMismatch {
            expected: prepared.continuation_id.clone(),
            actual: record.continuation_id.clone(),
        });
    }
    if record.current_attempt.attempt_id != prepared.attempt_id {
        return Err(AgentContinuationError::AttemptMismatch {
            expected: prepared.attempt_id.clone(),
            actual: record.current_attempt.attempt_id.clone(),
        });
    }
    if record.current_attempt.prompt_id != prepared.prompt_id
        || record.subagent_type != prepared.compatibility.subagent_type
        || record.model != prepared.compatibility.model
        || record.isolation != prepared.compatibility.isolation
        || record.effective_cwd != prepared.compatibility.effective_cwd
        || record.worktree != prepared.compatibility.worktree
        || record.compatibility_hash != prepared.compatibility.compatibility_hash
        || record.latest_task_id != prepared.latest_task_id
        || record.parent_task_id != prepared.parent_task_id
    {
        return Err(AgentContinuationError::CompatibilityMismatch);
    }
    Ok(())
}

fn ensure_record_resumable(
    record: &AgentContinuationRecord,
) -> Result<&AgentCheckpoint, AgentContinuationError> {
    if record_is_indeterminate(record) {
        return Err(AgentContinuationError::Indeterminate);
    }
    if !matches!(
        record.status,
        ContinuationStatus::Checkpointed
            | ContinuationStatus::Suspended
            | ContinuationStatus::Completed
            | ContinuationStatus::Failed
            | ContinuationStatus::Cancelled
    ) {
        return Err(AgentContinuationError::NotResumable {
            status: record.status,
        });
    }
    let checkpoint = record
        .checkpoint
        .as_ref()
        .ok_or(AgentContinuationError::CheckpointMissing)?;
    checkpoint.verify_digest()?;
    ensure_checkpoint_boundary_safe(checkpoint)?;
    Ok(checkpoint)
}

fn ensure_checkpoint_boundary_safe(
    checkpoint: &AgentCheckpoint,
) -> Result<(), AgentContinuationError> {
    if matches!(
        checkpoint.last_tool_boundary,
        Some(ToolBoundary::Indeterminate { .. })
    ) {
        return Err(AgentContinuationError::Indeterminate);
    }
    Ok(())
}

fn record_is_indeterminate(record: &AgentContinuationRecord) -> bool {
    record.status == ContinuationStatus::Indeterminate
        || record
            .active_tool_boundary
            .as_ref()
            .is_some_and(|boundary| matches!(boundary, ToolBoundary::Indeterminate { .. }))
        || record.checkpoint.as_ref().is_some_and(|checkpoint| {
            matches!(
                checkpoint.last_tool_boundary,
                Some(ToolBoundary::Indeterminate { .. })
            )
        })
}

fn record_has_live_owner(record: &AgentContinuationRecord, now_ms: i64) -> bool {
    record.current_attempt.owner_id.is_some()
        && record
            .current_attempt
            .lease_expires_at_ms
            .is_some_and(|expires_at_ms| expires_at_ms > now_ms)
}

fn reject_live_owner(
    record: &AgentContinuationRecord,
    now_ms: i64,
) -> Result<(), AgentContinuationError> {
    if let (Some(owner_id), Some(expires_at_ms)) = (
        record.current_attempt.owner_id.as_ref(),
        record.current_attempt.lease_expires_at_ms,
    ) && expires_at_ms > now_ms
    {
        return Err(AgentContinuationError::LeaseHeld {
            owner_id: owner_id.clone(),
            expires_at_ms,
        });
    }
    Ok(())
}

fn existing_owner_lease(
    record: &AgentContinuationRecord,
    owner_id: &str,
    now_ms: i64,
) -> Result<ContinuationLease, AgentContinuationError> {
    reject_live_owner_for_other(record, owner_id, now_ms)?;
    if record.current_attempt.owner_id.as_deref() != Some(owner_id)
        || record
            .current_attempt
            .lease_expires_at_ms
            .is_none_or(|expires_at_ms| expires_at_ms <= now_ms)
    {
        return Err(AgentContinuationError::LeaseExpired);
    }
    lease_from_record(record)
}

fn reject_live_owner_for_other(
    record: &AgentContinuationRecord,
    owner_id: &str,
    now_ms: i64,
) -> Result<(), AgentContinuationError> {
    if let (Some(current_owner_id), Some(expires_at_ms)) = (
        record.current_attempt.owner_id.as_ref(),
        record.current_attempt.lease_expires_at_ms,
    ) && current_owner_id != owner_id
        && expires_at_ms > now_ms
    {
        return Err(AgentContinuationError::LeaseHeld {
            owner_id: current_owner_id.clone(),
            expires_at_ms,
        });
    }
    Ok(())
}

fn validate_live_lease(
    record: &AgentContinuationRecord,
    lease: &ContinuationLease,
    now_ms: i64,
) -> Result<(), AgentContinuationError> {
    reject_live_owner_for_other(record, &lease.owner_id, now_ms)?;
    if record.current_attempt.owner_id.as_deref() != Some(lease.owner_id.as_str())
        || record
            .current_attempt
            .lease_expires_at_ms
            .is_none_or(|expires_at_ms| expires_at_ms <= now_ms)
    {
        return Err(AgentContinuationError::LeaseExpired);
    }
    Ok(())
}

fn lease_from_record(
    record: &AgentContinuationRecord,
) -> Result<ContinuationLease, AgentContinuationError> {
    let owner_id = record
        .current_attempt
        .owner_id
        .clone()
        .ok_or(AgentContinuationError::LeaseExpired)?;
    let expires_at_ms = record
        .current_attempt
        .lease_expires_at_ms
        .ok_or(AgentContinuationError::LeaseExpired)?;
    Ok(ContinuationLease {
        continuation_id: record.continuation_id.clone(),
        attempt_id: record.current_attempt.attempt_id.clone(),
        owner_id,
        lease_epoch: record.current_attempt.lease_epoch,
        expires_at_ms,
        revision: record.revision,
    })
}

fn terminal_status(terminal: &AgentTerminal) -> ContinuationStatus {
    match terminal {
        AgentTerminal::Completed { .. } => ContinuationStatus::Completed,
        AgentTerminal::Failed { .. } => ContinuationStatus::Failed,
        AgentTerminal::Cancelled { .. } => ContinuationStatus::Cancelled,
        AgentTerminal::Indeterminate { .. } => ContinuationStatus::Indeterminate,
    }
}

fn task_binding_label(task_id: Option<&str>) -> String {
    task_id.unwrap_or("<none>").to_string()
}

fn continuation_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Typed identity category used by stable UUID validation errors.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContinuationIdKind {
    /// Continuation lineage identity.
    Continuation,
    /// Execution attempt identity.
    Attempt,
    /// Safe checkpoint identity.
    Checkpoint,
    /// Accepted prompt idempotency identity.
    Prompt,
}

impl ContinuationIdKind {
    fn label(self) -> &'static str {
        match self {
            Self::Continuation => "continuation id",
            Self::Attempt => "attempt id",
            Self::Checkpoint => "checkpoint id",
            Self::Prompt => "prompt id",
        }
    }
}

/// Stable machine-readable category for continuation failures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentContinuationErrorCode {
    /// The requested continuation or compatibility selector does not exist.
    NotFound,
    /// A caller attempted to create an already-existing continuation identity.
    AlreadyExists,
    /// The identity is not parseable as a UUID.
    InvalidUuid,
    /// The identity UUID is not version 7.
    WrongUuidVersion,
    /// The requested continuation state edge is forbidden.
    InvalidTransition,
    /// The requested attempt state edge is forbidden.
    InvalidAttemptTransition,
    /// A commit targeted a different continuation.
    ContinuationMismatch,
    /// A commit targeted a stale or different attempt.
    AttemptMismatch,
    /// A commit used a stale continuation revision.
    RevisionConflict,
    /// The continuation revision counter cannot advance.
    RevisionExhausted,
    /// A commit used a stale lease epoch.
    LeaseEpochMismatch,
    /// The execution lease expired before the operation.
    LeaseExpired,
    /// Another owner still holds the execution lease.
    LeaseHeld,
    /// The continuation belongs to a different parent session.
    ParentMismatch,
    /// The continuation is bound to a different task lineage.
    TaskBindingMismatch,
    /// The requested runtime context is incompatible with the checkpoint.
    CompatibilityMismatch,
    /// A safe checkpoint is required but absent.
    CheckpointMissing,
    /// Unknown external side effects prohibit automatic continuation.
    Indeterminate,
    /// The continuation has no safe resumable state.
    NotResumable,
    /// The persisted schema version is unsupported.
    UnsupportedSchemaVersion,
    /// The persisted record is malformed or internally inconsistent.
    CorruptRecord,
    /// The persisted checkpoint digest does not match its payload.
    DigestMismatch,
    /// Durable storage failed.
    Persistence,
}

/// Stable error surface for continuation validation, fencing, and persistence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum AgentContinuationError {
    /// The requested continuation or compatibility selector does not exist.
    NotFound,
    /// A caller attempted to create an already-existing continuation identity.
    AlreadyExists {
        continuation_id: AgentContinuationId,
    },
    /// An identity is not parseable as a UUID.
    InvalidUuid { kind: ContinuationIdKind },
    /// An identity uses a UUID version other than v7.
    WrongUuidVersion {
        kind: ContinuationIdKind,
        found: usize,
    },
    /// A continuation lifecycle transition is forbidden.
    InvalidTransition {
        from: ContinuationStatus,
        to: ContinuationStatus,
    },
    /// An attempt lifecycle transition is forbidden.
    InvalidAttemptTransition {
        from: AttemptState,
        to: AttemptState,
    },
    /// The continuation component of a commit fence is stale or wrong.
    ContinuationMismatch {
        expected: AgentContinuationId,
        actual: AgentContinuationId,
    },
    /// The attempt component of a commit fence is stale or wrong.
    AttemptMismatch {
        expected: AgentAttemptId,
        actual: AgentAttemptId,
    },
    /// The revision component of a commit fence is stale.
    RevisionConflict {
        expected: ContinuationRevision,
        actual: ContinuationRevision,
    },
    /// The revision counter reached its maximum value.
    RevisionExhausted,
    /// The lease epoch component of a commit fence is stale.
    LeaseEpochMismatch { expected: u64, actual: u64 },
    /// The lease expired before the attempted operation.
    LeaseExpired,
    /// Another runtime owner still holds the lease.
    LeaseHeld {
        owner_id: String,
        expires_at_ms: i64,
    },
    /// The selected continuation belongs to another parent session.
    ParentMismatch { expected: String, actual: String },
    /// The selected continuation is bound to another task lineage.
    TaskBindingMismatch { expected: String, actual: String },
    /// The child runtime identity does not match the durable compatibility digest.
    CompatibilityMismatch,
    /// A safe checkpoint is required but absent.
    CheckpointMissing,
    /// Unknown external side effects prohibit automatic continuation.
    Indeterminate,
    /// No safe checkpoint or terminal state permits resumption.
    NotResumable { status: ContinuationStatus },
    /// The record schema cannot be decoded by this runtime.
    UnsupportedSchemaVersion { found: u32 },
    /// The record is structurally invalid or violates durable invariants.
    CorruptRecord { message: String },
    /// The checkpoint payload failed digest verification.
    DigestMismatch,
    /// Durable storage failed.
    Persistence { message: String },
}

impl AgentContinuationError {
    /// Returns the stable machine-readable category for this error.
    pub(crate) const fn code(&self) -> AgentContinuationErrorCode {
        match self {
            Self::NotFound => AgentContinuationErrorCode::NotFound,
            Self::AlreadyExists { .. } => AgentContinuationErrorCode::AlreadyExists,
            Self::InvalidUuid { .. } => AgentContinuationErrorCode::InvalidUuid,
            Self::WrongUuidVersion { .. } => AgentContinuationErrorCode::WrongUuidVersion,
            Self::InvalidTransition { .. } => AgentContinuationErrorCode::InvalidTransition,
            Self::InvalidAttemptTransition { .. } => {
                AgentContinuationErrorCode::InvalidAttemptTransition
            }
            Self::ContinuationMismatch { .. } => AgentContinuationErrorCode::ContinuationMismatch,
            Self::AttemptMismatch { .. } => AgentContinuationErrorCode::AttemptMismatch,
            Self::RevisionConflict { .. } => AgentContinuationErrorCode::RevisionConflict,
            Self::RevisionExhausted => AgentContinuationErrorCode::RevisionExhausted,
            Self::LeaseEpochMismatch { .. } => AgentContinuationErrorCode::LeaseEpochMismatch,
            Self::LeaseExpired => AgentContinuationErrorCode::LeaseExpired,
            Self::LeaseHeld { .. } => AgentContinuationErrorCode::LeaseHeld,
            Self::ParentMismatch { .. } => AgentContinuationErrorCode::ParentMismatch,
            Self::TaskBindingMismatch { .. } => AgentContinuationErrorCode::TaskBindingMismatch,
            Self::CompatibilityMismatch => AgentContinuationErrorCode::CompatibilityMismatch,
            Self::CheckpointMissing => AgentContinuationErrorCode::CheckpointMissing,
            Self::Indeterminate => AgentContinuationErrorCode::Indeterminate,
            Self::NotResumable { .. } => AgentContinuationErrorCode::NotResumable,
            Self::UnsupportedSchemaVersion { .. } => {
                AgentContinuationErrorCode::UnsupportedSchemaVersion
            }
            Self::CorruptRecord { .. } => AgentContinuationErrorCode::CorruptRecord,
            Self::DigestMismatch => AgentContinuationErrorCode::DigestMismatch,
            Self::Persistence { .. } => AgentContinuationErrorCode::Persistence,
        }
    }

    /// Returns the stable external continuation error code; it has no side effects and groups internal fence and corruption details into the contract surface.
    pub(crate) const fn contract_code(&self) -> &'static str {
        match self {
            Self::NotFound => CONTINUATION_NOT_FOUND_CODE,
            Self::ParentMismatch { .. } => CONTINUATION_PARENT_MISMATCH_CODE,
            Self::LeaseHeld { .. } => CONTINUATION_ACTIVE_CODE,
            Self::TaskBindingMismatch { .. } | Self::CompatibilityMismatch => {
                CONTINUATION_INCOMPATIBLE_CODE
            }
            Self::CheckpointMissing | Self::NotResumable { .. } => {
                CONTINUATION_CHECKPOINT_MISSING_CODE
            }
            Self::UnsupportedSchemaVersion { .. }
            | Self::CorruptRecord { .. }
            | Self::DigestMismatch => CONTINUATION_CHECKPOINT_CORRUPT_CODE,
            Self::Indeterminate => CONTINUATION_INDETERMINATE_CODE,
            Self::RevisionConflict { .. }
            | Self::ContinuationMismatch { .. }
            | Self::AttemptMismatch { .. }
            | Self::LeaseEpochMismatch { .. }
            | Self::LeaseExpired => CONTINUATION_REVISION_CONFLICT_CODE,
            Self::AlreadyExists { .. } => CONTINUATION_ALREADY_EXISTS_CODE,
            Self::InvalidUuid { .. } | Self::WrongUuidVersion { .. } => CONTINUATION_NOT_FOUND_CODE,
            Self::InvalidTransition { .. }
            | Self::InvalidAttemptTransition { .. }
            | Self::RevisionExhausted
            | Self::Persistence { .. } => CONTINUATION_PERSISTENCE_ERROR_CODE,
        }
    }
}

impl fmt::Display for AgentContinuationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("continuation was not found"),
            Self::AlreadyExists { continuation_id } => {
                write!(formatter, "continuation {continuation_id} already exists")
            }
            Self::InvalidUuid { kind } => {
                write!(formatter, "{} is not a valid UUID", kind.label())
            }
            Self::WrongUuidVersion { kind, found } => write!(
                formatter,
                "{} must be UUIDv7; found UUID version {found}",
                kind.label()
            ),
            Self::InvalidTransition { from, to } => write!(
                formatter,
                "invalid continuation transition from {} to {}",
                from.as_str(),
                to.as_str()
            ),
            Self::InvalidAttemptTransition { from, to } => write!(
                formatter,
                "invalid attempt transition from {} to {}",
                from.as_str(),
                to.as_str()
            ),
            Self::ContinuationMismatch { expected, actual } => write!(
                formatter,
                "continuation fence mismatch: expected {expected}, found {actual}"
            ),
            Self::AttemptMismatch { expected, actual } => write!(
                formatter,
                "attempt fence mismatch: expected {expected}, found {actual}"
            ),
            Self::RevisionConflict { expected, actual } => write!(
                formatter,
                "continuation revision conflict: expected {}, found {}",
                expected.get(),
                actual.get()
            ),
            Self::RevisionExhausted => formatter.write_str("continuation revision is exhausted"),
            Self::LeaseEpochMismatch { expected, actual } => write!(
                formatter,
                "continuation lease epoch mismatch: expected {expected}, found {actual}"
            ),
            Self::LeaseExpired => formatter.write_str("continuation lease has expired"),
            Self::LeaseHeld {
                owner_id,
                expires_at_ms,
            } => write!(
                formatter,
                "continuation lease is held by {owner_id} until {expires_at_ms}"
            ),
            Self::ParentMismatch { expected, actual } => write!(
                formatter,
                "continuation parent mismatch: expected {expected}, found {actual}"
            ),
            Self::TaskBindingMismatch { expected, actual } => write!(
                formatter,
                "continuation task binding mismatch: expected {expected}, found {actual}"
            ),
            Self::CompatibilityMismatch => {
                formatter.write_str("continuation context is incompatible with the checkpoint")
            }
            Self::CheckpointMissing => formatter.write_str("continuation has no safe checkpoint"),
            Self::Indeterminate => formatter.write_str(
                "continuation has indeterminate external side effects and cannot be resumed",
            ),
            Self::NotResumable { status } => write!(
                formatter,
                "continuation is not resumable from status {}",
                status.as_str()
            ),
            Self::UnsupportedSchemaVersion { found } => {
                write!(formatter, "unsupported continuation schema version {found}")
            }
            Self::CorruptRecord { message } => {
                write!(formatter, "continuation record is corrupt: {message}")
            }
            Self::DigestMismatch => formatter.write_str("continuation checkpoint digest mismatch"),
            Self::Persistence { message } => {
                write!(formatter, "continuation persistence failed: {message}")
            }
        }
    }
}

impl std::error::Error for AgentContinuationError {}

fn default_schema_version() -> u32 {
    AGENT_CONTINUATION_SCHEMA_VERSION
}

fn default_child_conversation_snapshot_schema_version() -> u32 {
    CHILD_CONVERSATION_SNAPSHOT_SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::conversation::Conversation;

    fn prepared_continuation(
        session: &str,
    ) -> (
        TaskRegistry,
        ChildAgentCoordinator,
        PreparedContinuation,
        ContinuationLease,
    ) {
        let registry = TaskRegistry::new(session.to_string());
        let task = registry.create_subagent_with_parent(
            "continuation test".to_string(),
            Some("general".to_string()),
            None,
        );
        let coordinator =
            ChildAgentCoordinator::with_owner_id(registry.clone(), format!("test-owner-{session}"))
                .expect("coordinator");
        let prepared = coordinator
            .create(CreateContinuationInput {
                continuation_id: Some(AgentContinuationId::new()),
                parent_task_id: None,
                task_id: task.id,
                prompt_id: AgentPromptId::new(),
                compatibility: ContinuationCompatibility {
                    subagent_type: "general".to_string(),
                    model: Some("test-model".to_string()),
                    isolation: SubagentIsolation::None,
                    effective_cwd: std::env::temp_dir().display().to_string(),
                    worktree: None,
                    compatibility_hash: Sha256Digest::new([7; 32]),
                },
            })
            .expect("prepare continuation");
        let lease = coordinator
            .acquire(&prepared)
            .expect("acquire continuation");
        (registry, coordinator, prepared, lease)
    }

    fn checkpoint(attempt_id: AgentAttemptId, sequence: u64, turn: u32) -> AgentCheckpoint {
        let conversation = Conversation::new();
        let (conversation, last_tool_boundary) =
            ChildConversationSnapshot::try_capture_safe(&conversation, turn + 1)
                .expect("capture checkpoint");
        let mut checkpoint = AgentCheckpoint {
            checkpoint_id: AgentCheckpointId::new(),
            attempt_id,
            sequence,
            conversation,
            turn,
            usage: BudgetUsage::default(),
            last_tool_boundary,
            created_at_ms: continuation_now_ms(),
            digest: Sha256Digest::new([0; 32]),
        };
        checkpoint.digest = checkpoint.computed_digest().expect("checkpoint digest");
        checkpoint
    }

    #[test]
    fn safe_checkpoint_clears_indeterminate_active_tool_boundary() {
        let (_registry, coordinator, _prepared, lease) =
            prepared_continuation("continuation-tool-boundary");
        let projection = coordinator
            .commit_tool_boundary(
                &lease,
                lease.revision,
                ToolBoundary::Indeterminate {
                    tool_call_id: Some("tool-1".to_string()),
                    reason: "external side effect may have started".to_string(),
                },
            )
            .expect("persist tool boundary");
        assert!(projection.indeterminate);

        let projection = coordinator
            .commit_checkpoint(
                &lease,
                projection.revision,
                checkpoint(lease.attempt_id.clone(), 0, 0),
            )
            .expect("commit safe checkpoint");
        assert!(!projection.indeterminate);
        assert!(projection.checkpoint_id.is_some());
    }

    #[test]
    fn expired_owner_without_checkpoint_reconciles_indeterminate() {
        let (_registry, coordinator, prepared, _lease) =
            prepared_continuation("continuation-orphan-unsafe");
        let record = coordinator
            .store
            .load_record(&prepared.continuation_id)
            .expect("load record")
            .expect("continuation record");
        coordinator
            .store
            .mutate_record(&prepared.continuation_id, record.revision, |record| {
                record.current_attempt.lease_expires_at_ms = Some(0);
                Ok(())
            })
            .expect("expire owner");

        let projection = coordinator
            .store
            .reconcile_expired_owners()
            .expect("reconcile owner")
            .into_iter()
            .find(|projection| projection.continuation_id == prepared.continuation_id)
            .expect("projection");
        assert!(projection.indeterminate);
        assert!(!projection.resumable);
        assert_eq!(projection.status, ContinuationStatus::Indeterminate);
    }

    #[test]
    fn expired_owner_with_checkpoint_reconciles_suspended_and_resumable() {
        let (_registry, coordinator, prepared, lease) =
            prepared_continuation("continuation-orphan-safe");
        let projection = coordinator
            .commit_checkpoint(
                &lease,
                lease.revision,
                checkpoint(lease.attempt_id.clone(), 0, 0),
            )
            .expect("checkpoint");
        coordinator
            .store
            .mutate_record(&prepared.continuation_id, projection.revision, |record| {
                record.current_attempt.lease_expires_at_ms = Some(0);
                Ok(())
            })
            .expect("expire owner");

        let projection = coordinator
            .store
            .reconcile_expired_owners()
            .expect("reconcile owner")
            .into_iter()
            .find(|projection| projection.continuation_id == prepared.continuation_id)
            .expect("projection");
        assert_eq!(projection.status, ContinuationStatus::Suspended);
        assert!(projection.resumable);
        assert!(!projection.indeterminate);
    }

    #[test]
    fn renewed_owner_can_commit_with_the_original_lease_identity() {
        let (_registry, coordinator, _prepared, lease) =
            prepared_continuation("continuation-renewed-owner");
        let renewed = coordinator
            .renew(&lease, lease.revision)
            .expect("renew continuation lease");

        let projection = coordinator
            .commit_tool_boundary(
                &lease,
                renewed.revision,
                ToolBoundary::SafeToRetry {
                    tool_call_id: Some("tool-after-renew".to_string()),
                },
            )
            .expect("commit with renewed owner fence");

        assert_eq!(projection.revision.get(), renewed.revision.get() + 1);
    }
}
