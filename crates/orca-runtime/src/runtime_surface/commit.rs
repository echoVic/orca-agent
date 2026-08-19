use super::{
    CommitClass, CommitProbe, ExclusiveOwnerLease, JsonlSurfaceCommitLedger, PreparedSurfaceCommit,
    RetryLocalProjectionToken, RetryProjectionToken, SurfaceBatchReceipt, SurfaceCommitBatch,
    SurfaceCommitBatchPreflightResult, SurfaceCommitId, SurfaceCommitLedger, SurfaceFactFamily,
    SurfaceLedgerError, SurfacePublisherPermit, SurfaceReduceMode, SurfaceReduceResult,
    SurfaceReducerError, SurfaceReducerState, SurfaceScope, ThreadOwnerEpoch, preflight_batch,
    reduce_batch,
};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::tasks::{
    LegacyActiveTaskAdoptionReceipt, LegacyActiveTaskAdoptionRecord,
    LegacyTerminalTaskReconciliationReceipt,
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum ManualCompactionItemKey {
    System {
        content: String,
        pinned: bool,
    },
    User {
        content: String,
        pinned: bool,
    },
    AssistantText {
        content: String,
        pinned: bool,
    },
    AssistantReasoning {
        content: String,
        pinned: bool,
    },
    Tool {
        tool_call_id: String,
        content: String,
        pinned: bool,
    },
}

fn manual_compaction_conversation_keys(
    conversation: &orca_core::conversation::Conversation,
) -> Vec<ManualCompactionItemKey> {
    let mut keys = Vec::new();
    for message in &conversation.messages {
        match message {
            orca_core::conversation::Message::System { content, pinned } => {
                keys.push(ManualCompactionItemKey::System {
                    content: content.clone(),
                    pinned: *pinned,
                });
            }
            orca_core::conversation::Message::User { content, pinned } => {
                keys.push(ManualCompactionItemKey::User {
                    content: content.clone(),
                    pinned: *pinned,
                });
            }
            orca_core::conversation::Message::Assistant {
                content,
                reasoning_content,
                pinned,
                ..
            } => {
                if let Some(content) = content {
                    keys.push(ManualCompactionItemKey::AssistantText {
                        content: content.clone(),
                        pinned: *pinned,
                    });
                }
                if let Some(content) = reasoning_content {
                    keys.push(ManualCompactionItemKey::AssistantReasoning {
                        content: content.clone(),
                        pinned: *pinned,
                    });
                }
            }
            orca_core::conversation::Message::Tool {
                tool_call_id,
                content,
                pinned,
                ..
            } => keys.push(ManualCompactionItemKey::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
                pinned: *pinned,
            }),
        }
    }
    keys
}

fn manual_compaction_surface_item_key(
    item: &super::SurfaceItem,
) -> Option<ManualCompactionItemKey> {
    match item {
        super::SurfaceItem::SystemMessage {
            content, pinned, ..
        } => Some(ManualCompactionItemKey::System {
            content: content.as_str().to_string(),
            pinned: *pinned,
        }),
        super::SurfaceItem::UserMessage { input, pinned, .. } => {
            manual_compaction_surface_user_input(input).map(|content| {
                ManualCompactionItemKey::User {
                    content: content.to_string(),
                    pinned: *pinned,
                }
            })
        }
        super::SurfaceItem::AssistantMessage { text, pinned, .. }
        | super::SurfaceItem::AssistantPlan { text, pinned, .. } => {
            Some(ManualCompactionItemKey::AssistantText {
                content: text.as_str().to_string(),
                pinned: *pinned,
            })
        }
        super::SurfaceItem::AssistantReasoning {
            content, pinned, ..
        } => Some(ManualCompactionItemKey::AssistantReasoning {
            content: content.as_str().to_string(),
            pinned: *pinned,
        }),
        super::SurfaceItem::ToolResultMessage {
            tool_call_id,
            content,
            pinned,
            ..
        } => Some(ManualCompactionItemKey::Tool {
            tool_call_id: tool_call_id.as_str().to_string(),
            content: content.as_str().to_string(),
            pinned: *pinned,
        }),
    }
}

fn manual_compaction_key_matches(
    candidate: &ManualCompactionItemKey,
    target: &ManualCompactionItemKey,
) -> bool {
    if candidate == target {
        return true;
    }
    let redacted_matches = |candidate: &str, target: &str| {
        crate::thread_store::redact_sensitive_text(candidate) == target
    };
    match (candidate, target) {
        (
            ManualCompactionItemKey::System {
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::System {
                content: target,
                pinned: target_pinned,
            },
        )
        | (
            ManualCompactionItemKey::User {
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::User {
                content: target,
                pinned: target_pinned,
            },
        )
        | (
            ManualCompactionItemKey::AssistantText {
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::AssistantText {
                content: target,
                pinned: target_pinned,
            },
        )
        | (
            ManualCompactionItemKey::AssistantReasoning {
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::AssistantReasoning {
                content: target,
                pinned: target_pinned,
            },
        ) => candidate_pinned == target_pinned && redacted_matches(candidate, target),
        (
            ManualCompactionItemKey::Tool {
                tool_call_id: candidate_id,
                content: candidate,
                pinned: candidate_pinned,
            },
            ManualCompactionItemKey::Tool {
                tool_call_id: target_id,
                content: target,
                pinned: target_pinned,
            },
        ) => {
            candidate_id == target_id
                && candidate_pinned == target_pinned
                && redacted_matches(candidate, target)
        }
        _ => false,
    }
}

fn manual_compaction_surface_user_input(input: &super::SurfaceUserInputState) -> Option<&str> {
    match input {
        super::SurfaceUserInputState::Resolved {
            fact: super::SurfaceResolvedInputFact::Replayable { input, .. },
        } => Some(input.canonical_text.as_str()),
        super::SurfaceUserInputState::Resolved {
            fact:
                super::SurfaceResolvedInputFact::NonReplayable {
                    presentation: super::SurfaceInputPresentation::Visible { text },
                    ..
                },
        } => Some(text.as_str()),
        super::SurfaceUserInputState::Pending { .. }
        | super::SurfaceUserInputState::ResolutionFailed { .. }
        | super::SurfaceUserInputState::Resolved {
            fact:
                super::SurfaceResolvedInputFact::NonReplayable {
                    presentation: super::SurfaceInputPresentation::Redacted,
                    ..
                },
        } => None,
    }
}

fn manual_compaction_surface_item_id(item: &super::SurfaceItem) -> &super::SurfaceItemId {
    match item {
        super::SurfaceItem::UserMessage { id, .. }
        | super::SurfaceItem::SystemMessage { id, .. }
        | super::SurfaceItem::AssistantMessage { id, .. }
        | super::SurfaceItem::AssistantReasoning { id, .. }
        | super::SurfaceItem::AssistantPlan { id, .. }
        | super::SurfaceItem::ToolResultMessage { id, .. } => id,
    }
}

pub(crate) fn manual_compaction_item_patches(
    items: &[super::SurfaceItem],
    conversation: &orca_core::conversation::Conversation,
) -> Option<Vec<super::ItemPatch>> {
    let keys = manual_compaction_conversation_keys(conversation);
    let mut assignments = vec![None; keys.len()];
    let mut required = vec![false; keys.len()];
    let mut used = vec![false; items.len()];
    for (target_index, key) in keys.iter().enumerate().rev() {
        required[target_index] = match key {
            ManualCompactionItemKey::System { .. } => true,
            ManualCompactionItemKey::User { .. } => items
                .iter()
                .any(|item| matches!(item, super::SurfaceItem::UserMessage { .. })),
            ManualCompactionItemKey::AssistantText { .. } => items.iter().any(|item| {
                matches!(
                    item,
                    super::SurfaceItem::AssistantMessage { .. }
                        | super::SurfaceItem::AssistantPlan { .. }
                )
            }),
            ManualCompactionItemKey::AssistantReasoning { .. } => items
                .iter()
                .any(|item| matches!(item, super::SurfaceItem::AssistantReasoning { .. })),
            ManualCompactionItemKey::Tool { .. } => items
                .iter()
                .any(|item| matches!(item, super::SurfaceItem::ToolResultMessage { .. })),
        };
        if let Some(source_index) = items.iter().enumerate().rev().find_map(|(index, item)| {
            (!used[index]
                && manual_compaction_surface_item_key(item)
                    .as_ref()
                    .is_some_and(|candidate| manual_compaction_key_matches(candidate, key)))
            .then_some(index)
        }) {
            used[source_index] = true;
            assignments[target_index] = Some(items[source_index].clone());
            continue;
        }
        let replacement = match key {
            ManualCompactionItemKey::System { content, pinned } => {
                Some(super::SurfaceItem::SystemMessage {
                    id: super::SurfaceItemId::new(),
                    content: super::DisplayText::new(content.clone()),
                    pinned: *pinned,
                    origin: super::SurfaceItemOrigin::HistoryMaterialization,
                })
            }
            ManualCompactionItemKey::Tool {
                tool_call_id,
                content,
                pinned,
            } => items.iter().enumerate().rev().find_map(|(index, item)| {
                if used[index] {
                    return None;
                }
                let super::SurfaceItem::ToolResultMessage {
                    id,
                    turn_id,
                    tool_call_id: existing_id,
                    terminal,
                    ..
                } = item
                else {
                    return None;
                };
                (existing_id.as_str() == tool_call_id).then(|| {
                    used[index] = true;
                    super::SurfaceItem::ToolResultMessage {
                        id: id.clone(),
                        turn_id: turn_id.clone(),
                        tool_call_id: existing_id.clone(),
                        content: super::DisplayText::new(content.clone()),
                        terminal: terminal.clone(),
                        pinned: *pinned,
                    }
                })
            }),
            ManualCompactionItemKey::User { .. }
            | ManualCompactionItemKey::AssistantText { .. }
            | ManualCompactionItemKey::AssistantReasoning { .. } => None,
        };
        assignments[target_index] = replacement;
    }
    if assignments
        .iter()
        .zip(required.iter())
        .any(|(assignment, required)| *required && assignment.is_none())
    {
        return None;
    }
    let mut patches = items
        .iter()
        .map(|item| super::ItemPatch::Removed {
            item_id: manual_compaction_surface_item_id(item).clone(),
            reason: super::ItemRemovalReason::Compacted,
        })
        .collect::<Vec<_>>();
    patches.extend(
        assignments
            .into_iter()
            .flatten()
            .map(|item| super::ItemPatch::Added { item }),
    );
    Some(patches)
}

pub(super) fn manual_compaction_item_rebuild_paired(
    snapshot: &super::SurfaceSnapshot,
    batch: &super::SurfaceCommitBatch,
) -> bool {
    let completes_manual_compaction = batch.events.as_slice().iter().any(|event| {
        matches!(
            &event.event,
            super::SurfaceEvent::Context(super::SurfaceContextSnapshot {
                compaction: super::CompactionState::Completed {
                    reason: super::CompactionReason::Manual,
                    ..
                },
                ..
            })
        )
    });
    if !completes_manual_compaction {
        return false;
    }
    let removed = batch
        .events
        .as_slice()
        .iter()
        .filter_map(|event| match &event.event {
            super::SurfaceEvent::Item(super::ItemPatch::Removed {
                item_id,
                reason: super::ItemRemovalReason::Compacted,
            }) => Some(item_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    removed.len() == snapshot.items.len()
        && snapshot.items.iter().all(|item| {
            removed
                .iter()
                .filter(|item_id| **item_id == manual_compaction_surface_item_id(item))
                .count()
                == 1
        })
        && batch
            .events
            .as_slice()
            .iter()
            .all(|event| match &event.event {
                super::SurfaceEvent::Item(super::ItemPatch::Removed {
                    item_id,
                    reason: super::ItemRemovalReason::Compacted,
                }) => snapshot
                    .items
                    .iter()
                    .any(|item| manual_compaction_surface_item_id(item) == item_id),
                super::SurfaceEvent::Item(super::ItemPatch::Added { item }) => {
                    let item_id = manual_compaction_surface_item_id(item);
                    match snapshot
                        .items
                        .iter()
                        .find(|existing| manual_compaction_surface_item_id(existing) == item_id)
                    {
                        Some(existing) if existing == item => true,
                        Some(super::SurfaceItem::ToolResultMessage {
                            turn_id,
                            tool_call_id,
                            terminal,
                            ..
                        }) => matches!(
                            item,
                            super::SurfaceItem::ToolResultMessage {
                                turn_id: added_turn,
                                tool_call_id: added_tool,
                                terminal: added_terminal,
                                ..
                            } if added_turn == turn_id
                                && added_tool == tool_call_id
                                && added_terminal == terminal
                        ),
                        Some(_) => false,
                        None => matches!(
                            item,
                            super::SurfaceItem::SystemMessage {
                                origin: super::SurfaceItemOrigin::HistoryMaterialization,
                                ..
                            }
                        ),
                    }
                }
                super::SurfaceEvent::Item(_) => false,
                _ => true,
            })
        && {
            let added_ids = batch
                .events
                .as_slice()
                .iter()
                .filter_map(|event| match &event.event {
                    super::SurfaceEvent::Item(super::ItemPatch::Added { item }) => {
                        Some(manual_compaction_surface_item_id(item))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            added_ids.iter().enumerate().all(|(index, item_id)| {
                added_ids[index + 1..]
                    .iter()
                    .all(|other| *other != *item_id)
            })
        }
}

#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceCommitError {
    OversizedBatch,
    InvalidBatch(SurfaceReducerError),
    StaleOwnerEpoch,
    StalePublisherPermit,
    CursorRangeAlreadyConsumed,
    Ledger(SurfaceLedgerError),
    Settlement(super::SettlementError),
    ProjectionPending { token: RetryProjectionToken },
}

impl std::fmt::Debug for SurfaceCommitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OversizedBatch => formatter.write_str("OversizedBatch"),
            Self::InvalidBatch(error) => {
                formatter.debug_tuple("InvalidBatch").field(error).finish()
            }
            Self::StaleOwnerEpoch => formatter.write_str("StaleOwnerEpoch"),
            Self::StalePublisherPermit => formatter.write_str("StalePublisherPermit"),
            Self::CursorRangeAlreadyConsumed => formatter.write_str("CursorRangeAlreadyConsumed"),
            Self::Ledger(error) => formatter.debug_tuple("Ledger").field(error).finish(),
            Self::Settlement(error) => formatter.debug_tuple("Settlement").field(error).finish(),
            Self::ProjectionPending { .. } => formatter.write_str("ProjectionPending"),
        }
    }
}

#[derive(Clone)]
pub struct SurfaceProjectionContext {
    pub request_id: super::SurfaceRequestId,
    pub target: super::MutationTarget,
    pub fact_family: SurfaceFactFamily,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceCommitApplied {
    pub receipt: SurfaceBatchReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HistoricalToolResultCommitAuthority {
    current_owner_epoch: ThreadOwnerEpoch,
    current_incarnation: super::SurfaceIncarnation,
    historical_fence: super::SurfaceOperationFence,
    invocation_id: super::SurfaceToolCallId,
    invocation_started: super::ToolInvocationStartedReceiptV1,
    expected_projection_revision: super::ToolInvocationRevision,
}

impl HistoricalToolResultCommitAuthority {
    pub(crate) fn historical_fence(&self) -> &super::SurfaceOperationFence {
        &self.historical_fence
    }

    pub(crate) fn invocation_id(&self) -> &super::SurfaceToolCallId {
        &self.invocation_id
    }

    pub(crate) const fn expected_projection_revision(&self) -> super::ToolInvocationRevision {
        self.expected_projection_revision
    }
}

struct ColdOwnerTakeoverAuthority {
    previous_owner_epoch: ThreadOwnerEpoch,
    previous_incarnation: super::SurfaceIncarnation,
    current_owner_epoch: ThreadOwnerEpoch,
    new_incarnation: Option<super::SurfaceIncarnation>,
    recoverable_operations: Vec<super::SurfaceOperationId>,
}

#[derive(Default)]
struct BoundedPublicationSuffix {
    batches: VecDeque<SurfaceCommitBatch>,
    encoded_bytes: VecDeque<u64>,
    events: u64,
    bytes: u64,
}

impl BoundedPublicationSuffix {
    fn from_committed(committed: Vec<SurfaceCommitBatch>) -> Self {
        let mut suffix = Self::default();
        let mut expected_after = None;
        for batch in committed.into_iter().rev() {
            if expected_after
                .as_ref()
                .is_some_and(|expected| expected != &batch.cursor_after)
            {
                break;
            }
            let batch_events = batch.event_count as u64;
            let batch_bytes = super::canonical_batch_encoded_bytes(&batch);
            if suffix.events.saturating_add(batch_events) > super::SURFACE_RETAINED_EVENT_LIMIT
                || suffix.bytes.saturating_add(batch_bytes) > super::SURFACE_RETAINED_BYTE_LIMIT
            {
                break;
            }
            expected_after = Some(batch.cursor_before.clone());
            suffix.events += batch_events;
            suffix.bytes += batch_bytes;
            suffix.batches.push_front(batch);
            suffix.encoded_bytes.push_front(batch_bytes);
        }
        suffix
    }

    fn push(&mut self, batch: &SurfaceCommitBatch) {
        if self
            .batches
            .back()
            .is_some_and(|previous| previous.cursor_after != batch.cursor_before)
        {
            self.clear();
        }
        let batch_bytes = super::canonical_batch_encoded_bytes(batch);
        self.events = self.events.saturating_add(batch.event_count as u64);
        self.bytes = self.bytes.saturating_add(batch_bytes);
        self.batches.push_back(batch.clone());
        self.encoded_bytes.push_back(batch_bytes);
        while self.events > super::SURFACE_RETAINED_EVENT_LIMIT
            || self.bytes > super::SURFACE_RETAINED_BYTE_LIMIT
        {
            let Some(expired) = self.batches.pop_front() else {
                break;
            };
            let expired_bytes = self
                .encoded_bytes
                .pop_front()
                .expect("publication bytes track every retained batch");
            self.events = self.events.saturating_sub(expired.event_count as u64);
            self.bytes = self.bytes.saturating_sub(expired_bytes);
        }
    }

    fn make_contiguous(&mut self) -> &[SurfaceCommitBatch] {
        self.batches.make_contiguous()
    }

    fn clear(&mut self) {
        self.batches.clear();
        self.encoded_bytes.clear();
        self.events = 0;
        self.bytes = 0;
    }
}

impl ColdOwnerTakeoverAuthority {
    fn authorizes_transition(
        &self,
        snapshot: &super::SurfaceSnapshot,
        new_incarnation: &super::SurfaceIncarnation,
        new_owner_epoch: &ThreadOwnerEpoch,
    ) -> bool {
        if new_owner_epoch != &self.current_owner_epoch {
            return false;
        }
        match &self.new_incarnation {
            Some(expected) => {
                new_incarnation == expected
                    && snapshot.thread.owner_epoch == self.current_owner_epoch
                    && snapshot.cursor.incarnation == *expected
            }
            None => {
                snapshot.thread.owner_epoch == self.previous_owner_epoch
                    && snapshot.cursor.incarnation == self.previous_incarnation
                    && new_incarnation != &self.previous_incarnation
            }
        }
    }

    fn authorizes(
        &self,
        operation_id: &super::SurfaceOperationId,
        snapshot: &super::SurfaceSnapshot,
        new_incarnation: &super::SurfaceIncarnation,
        new_owner_epoch: &ThreadOwnerEpoch,
    ) -> bool {
        self.recoverable_operations.contains(operation_id)
            && self.authorizes_transition(snapshot, new_incarnation, new_owner_epoch)
    }

    fn authorizes_historical_commit(
        &self,
        fence: &super::SurfaceOperationFence,
        snapshot: &super::SurfaceSnapshot,
        current_incarnation: &super::SurfaceIncarnation,
        current_owner_epoch: ThreadOwnerEpoch,
    ) -> bool {
        self.new_incarnation.as_ref() == Some(current_incarnation)
            && self.current_owner_epoch == current_owner_epoch
            && snapshot.thread.owner_epoch == current_owner_epoch
            && snapshot.cursor.incarnation == *current_incarnation
            && fence.thread_owner_epoch < current_owner_epoch
            && self.recoverable_operations.contains(&fence.operation_id)
    }
}

enum OwnerLeaseAuthority<'owner> {
    Borrowed(&'owner ExclusiveOwnerLease),
    Owned(ExclusiveOwnerLease),
}

enum BatchCommitAuthority<'permit> {
    Single(&'permit SurfacePublisherPermit),
    ActiveTaskAdoption {
        actor: &'permit SurfacePublisherPermit,
        receipt: &'permit LegacyActiveTaskAdoptionReceipt,
    },
    RecoveredActiveTaskAdoption {
        actor: &'permit SurfacePublisherPermit,
    },
    TaskReconciliation {
        actor: &'permit SurfacePublisherPermit,
        receipt: &'permit LegacyTerminalTaskReconciliationReceipt,
    },
    RecoveredTaskReconciliation {
        actor: &'permit SurfacePublisherPermit,
    },
    ActorGoal {
        actor: &'permit SurfacePublisherPermit,
        goal: &'permit SurfacePublisherPermit,
    },
    ActorGoals {
        actor: &'permit SurfacePublisherPermit,
        first_goal: &'permit SurfacePublisherPermit,
        second_goal: &'permit SurfacePublisherPermit,
    },
    ActorGenerationTerminalization {
        actor: &'permit SurfacePublisherPermit,
        generation: &'permit SurfacePublisherPermit,
    },
    ActorGenerationInterrupt {
        actor: &'permit SurfacePublisherPermit,
        generation: &'permit SurfacePublisherPermit,
    },
    ActorFinalizerTaskTerminal {
        actor: &'permit SurfacePublisherPermit,
        finalizer: &'permit SurfacePublisherPermit,
    },
    ActorBackgroundControl {
        actor: &'permit SurfacePublisherPermit,
        background: &'permit SurfacePublisherPermit,
    },
    ProviderBackgroundSuspend {
        actor: &'permit SurfacePublisherPermit,
        background: &'permit SurfacePublisherPermit,
    },
    ProviderBackgroundInteractionRoute {
        actor: &'permit SurfacePublisherPermit,
        background: &'permit SurfacePublisherPermit,
    },
    ProviderBackgroundInteractionResolution {
        actor: &'permit SurfacePublisherPermit,
        background: &'permit SurfacePublisherPermit,
        expected_safe_projection: Option<&'permit super::SurfaceInteractionSafeProjection>,
    },
    ProviderBackgroundResume {
        actor: &'permit SurfacePublisherPermit,
        background: &'permit SurfacePublisherPermit,
    },
    WorkflowBackgroundStop {
        actor: &'permit SurfacePublisherPermit,
        background: &'permit SurfacePublisherPermit,
        finalizer: &'permit SurfacePublisherPermit,
    },
    #[allow(dead_code)]
    LiveGenerationSuspend {
        actor: &'permit SurfacePublisherPermit,
        generation: &'permit SurfacePublisherPermit,
    },
    LiveGenerationStop {
        generation: &'permit SurfacePublisherPermit,
        finalizer: &'permit SurfacePublisherPermit,
    },
    GoalGenerationStop {
        finished_goal: &'permit SurfacePublisherPermit,
        verification_goal: Option<&'permit SurfacePublisherPermit>,
        decision_goal: &'permit SurfacePublisherPermit,
        generation: &'permit SurfacePublisherPermit,
        finalizer: &'permit SurfacePublisherPermit,
    },
    GoalGenerationContinue {
        actor: &'permit SurfacePublisherPermit,
        finished_goal: &'permit SurfacePublisherPermit,
        verification_goal: Option<&'permit SurfacePublisherPermit>,
        decision_goal: &'permit SurfacePublisherPermit,
        predecessor: &'permit SurfacePublisherPermit,
    },
    HistoricalToolResult {
        authority: &'permit HistoricalToolResultCommitAuthority,
    },
}

enum RecoveredBatchAuthority {
    Single(SurfacePublisherPermit),
    ActiveTaskAdoption {
        actor: SurfacePublisherPermit,
    },
    TaskReconciliation {
        actor: SurfacePublisherPermit,
    },
    ActorGoal {
        actor: SurfacePublisherPermit,
        goal: SurfacePublisherPermit,
    },
    ActorGoals {
        actor: SurfacePublisherPermit,
        first_goal: SurfacePublisherPermit,
        second_goal: SurfacePublisherPermit,
    },
    ActorGenerationTerminalization {
        actor: SurfacePublisherPermit,
        generation: SurfacePublisherPermit,
    },
    ActorGenerationInterrupt {
        actor: SurfacePublisherPermit,
        generation: SurfacePublisherPermit,
    },
    ActorFinalizerTaskTerminal {
        actor: SurfacePublisherPermit,
        finalizer: SurfacePublisherPermit,
    },
    ActorBackgroundControl {
        actor: SurfacePublisherPermit,
        background: SurfacePublisherPermit,
    },
    ProviderBackgroundSuspend {
        actor: SurfacePublisherPermit,
        background: SurfacePublisherPermit,
    },
    ProviderBackgroundInteractionRoute {
        actor: SurfacePublisherPermit,
        background: SurfacePublisherPermit,
    },
    ProviderBackgroundInteractionResolution {
        actor: SurfacePublisherPermit,
        background: SurfacePublisherPermit,
    },
    ProviderBackgroundResume {
        actor: SurfacePublisherPermit,
        background: SurfacePublisherPermit,
    },
    WorkflowBackgroundStop {
        actor: SurfacePublisherPermit,
        background: SurfacePublisherPermit,
        finalizer: SurfacePublisherPermit,
    },
    GoalGenerationStop {
        finished_goal: SurfacePublisherPermit,
        verification_goal: Option<SurfacePublisherPermit>,
        decision_goal: SurfacePublisherPermit,
        generation: SurfacePublisherPermit,
        finalizer: SurfacePublisherPermit,
    },
    GoalGenerationContinue {
        actor: SurfacePublisherPermit,
        finished_goal: SurfacePublisherPermit,
        verification_goal: Option<SurfacePublisherPermit>,
        decision_goal: SurfacePublisherPermit,
        predecessor: SurfacePublisherPermit,
    },
}

impl OwnerLeaseAuthority<'_> {
    fn lease(&self) -> &ExclusiveOwnerLease {
        match self {
            Self::Borrowed(lease) => lease,
            Self::Owned(lease) => lease,
        }
    }
}

pub struct RuntimeCommitCoordinator<'owner, L> {
    ledger: L,
    state: SurfaceReducerState,
    surface_hub: Option<super::SurfaceHub>,
    recovered_publications: BoundedPublicationSuffix,
    owner_lease: OwnerLeaseAuthority<'owner>,
    owner_epoch: ThreadOwnerEpoch,
    actor_control_permit: SurfacePublisherPermit,
    issued_permits: Vec<SurfacePublisherPermit>,
    next_sequence: u64,
    incomplete: Option<SurfaceCommitBatch>,
    recovered_prepared: Option<SurfaceCommitBatch>,
    cold_takeover_authority: Option<ColdOwnerTakeoverAuthority>,
    pending_projection: Option<(RetryProjectionToken, SurfaceCommitBatch)>,
    #[cfg(test)]
    projection_failure_injected: bool,
}

fn recovered_cold_takeover_authority(
    state: &SurfaceReducerState,
    current_owner_epoch: ThreadOwnerEpoch,
    materialized: Option<ColdOwnerTakeoverAuthority>,
) -> Option<ColdOwnerTakeoverAuthority> {
    let snapshot = state.snapshot();
    if snapshot.thread.owner_epoch < current_owner_epoch {
        return Some(ColdOwnerTakeoverAuthority {
            previous_owner_epoch: snapshot.thread.owner_epoch,
            previous_incarnation: snapshot.cursor.incarnation.clone(),
            current_owner_epoch,
            new_incarnation: None,
            recoverable_operations: snapshot_operation_ids(snapshot),
        });
    }
    (snapshot.thread.owner_epoch == current_owner_epoch)
        .then_some(materialized)
        .flatten()
}

fn snapshot_operation_ids(snapshot: &super::SurfaceSnapshot) -> Vec<super::SurfaceOperationId> {
    snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .map(|operation| operation.operation_id.clone())
        .collect()
}

fn materialized_takeover_authority(
    state: &SurfaceReducerState,
    batch: &SurfaceCommitBatch,
    current_owner_epoch: ThreadOwnerEpoch,
) -> Option<ColdOwnerTakeoverAuthority> {
    let transition = batch
        .events
        .as_slice()
        .iter()
        .find_map(|event| match &event.event {
            super::SurfaceEvent::Session(super::SessionPatch::OwnerEpochChanged {
                previous,
                next,
            }) if next == &current_owner_epoch => Some((*previous, *next)),
            _ => None,
        })?;
    (transition.0 < transition.1
        && state.snapshot().thread.owner_epoch == transition.0
        && batch.cursor_before.incarnation != batch.cursor_after.incarnation)
        .then(|| ColdOwnerTakeoverAuthority {
            previous_owner_epoch: transition.0,
            previous_incarnation: batch.cursor_before.incarnation.clone(),
            current_owner_epoch: transition.1,
            new_incarnation: Some(batch.cursor_after.incarnation.clone()),
            recoverable_operations: snapshot_operation_ids(state.snapshot()),
        })
}

impl<'owner, L: SurfaceCommitLedger> RuntimeCommitCoordinator<'owner, L> {
    pub fn new_with_owner_lease(
        ledger: L,
        state: SurfaceReducerState,
        owner_lease: &'owner ExclusiveOwnerLease,
    ) -> Result<Self, SurfaceCommitError> {
        Self::new_with_authority(ledger, state, OwnerLeaseAuthority::Borrowed(owner_lease))
    }

    fn new_with_authority(
        ledger: L,
        state: SurfaceReducerState,
        owner_lease: OwnerLeaseAuthority<'owner>,
    ) -> Result<Self, SurfaceCommitError> {
        let lease = owner_lease.lease();
        if !lease.authorizes_thread(&state.snapshot().thread.thread_id) {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        let next_sequence = state.snapshot().cursor.next_seq.get();
        let owner_epoch = ThreadOwnerEpoch::new(lease.owner_epoch());
        let actor_control_permit = SurfacePublisherPermit::ActorControl {
            permit_id: next_permit_id(),
            thread_id: state.snapshot().thread.thread_id.clone(),
            owner_epoch,
        };
        Ok(Self {
            ledger,
            state,
            surface_hub: None,
            recovered_publications: BoundedPublicationSuffix::default(),
            owner_lease,
            owner_epoch,
            issued_permits: vec![actor_control_permit.clone()],
            actor_control_permit,
            next_sequence,
            incomplete: None,
            recovered_prepared: None,
            cold_takeover_authority: None,
            pending_projection: None,
            #[cfg(test)]
            projection_failure_injected: false,
        })
    }
}

impl<L: SurfaceCommitLedger> RuntimeCommitCoordinator<'static, L> {
    pub fn new_with_owned_lease(
        ledger: L,
        state: SurfaceReducerState,
        owner_lease: ExclusiveOwnerLease,
    ) -> Result<Self, SurfaceCommitError> {
        Self::new_with_authority(ledger, state, OwnerLeaseAuthority::Owned(owner_lease))
    }
}

impl<'owner> RuntimeCommitCoordinator<'owner, JsonlSurfaceCommitLedger> {
    pub fn recover(
        ledger: JsonlSurfaceCommitLedger,
        state: SurfaceReducerState,
        owner_lease: &'owner ExclusiveOwnerLease,
    ) -> Result<Self, SurfaceCommitError> {
        Self::recover_with_authority(ledger, state, OwnerLeaseAuthority::Borrowed(owner_lease))
    }

    fn recover_with_authority(
        ledger: JsonlSurfaceCommitLedger,
        mut state: SurfaceReducerState,
        owner_lease: OwnerLeaseAuthority<'owner>,
    ) -> Result<Self, SurfaceCommitError> {
        let lease = owner_lease.lease();
        if !lease.authorizes_thread(&state.snapshot().thread.thread_id) {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        let recovered = ledger
            .recover_batches()
            .map_err(SurfaceCommitError::Ledger)?;
        let committed = recovered.committed;
        let prepared = recovered.prepared;
        let current_owner_epoch = ThreadOwnerEpoch::new(lease.owner_epoch());
        if let Some(first) = committed.first().or(prepared.as_ref())
            && first.cursor_before.next_seq.get() == 0
        {
            let initial_owner_epoch = first
                .events
                .as_slice()
                .iter()
                .find_map(|event| match &event.event {
                    super::SurfaceEvent::Session(super::SessionPatch::OwnerEpochChanged {
                        previous,
                        ..
                    }) => Some(*previous),
                    _ => None,
                })
                .unwrap_or_else(|| match first.commit_class {
                    CommitClass::Recorded {
                        thread_owner_epoch, ..
                    } => thread_owner_epoch,
                    CommitClass::Ephemeral { .. } => {
                        unreachable!("JSONL surface ledger cannot contain ephemeral batches")
                    }
                });
            state
                .align_rematerialization_baseline(first.cursor_before.clone(), initial_owner_epoch);
        }
        let mut materialized_takeover = None;
        for batch in &committed {
            let candidate_takeover =
                materialized_takeover_authority(&state, batch, current_owner_epoch);
            state = match reduce_batch(SurfaceReduceMode::Rematerialization, &state, batch) {
                SurfaceReduceResult::Applied { state } => state,
                SurfaceReduceResult::AlreadyApplied { .. } => state,
                SurfaceReduceResult::Rejected { error } => {
                    return Err(SurfaceCommitError::InvalidBatch(error));
                }
            };
            if candidate_takeover.is_some() {
                materialized_takeover = candidate_takeover;
            }
        }

        let cold_takeover_authority =
            recovered_cold_takeover_authority(&state, current_owner_epoch, materialized_takeover);
        let recovered_publications = BoundedPublicationSuffix::from_committed(committed);
        let mut coordinator = Self::new_with_authority(ledger, state, owner_lease)?;
        coordinator.recovered_publications = recovered_publications;
        coordinator.cold_takeover_authority = cold_takeover_authority;
        if let Some(batch) = prepared {
            match reduce_batch(
                SurfaceReduceMode::Rematerialization,
                &coordinator.state,
                &batch,
            ) {
                SurfaceReduceResult::Applied { .. } => {}
                SurfaceReduceResult::AlreadyApplied { .. } => {
                    return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
                }
                SurfaceReduceResult::Rejected { error } => {
                    return Err(SurfaceCommitError::InvalidBatch(error));
                }
            }
            coordinator.next_sequence = batch.cursor_after.next_seq.get();
            coordinator.incomplete = Some(batch.clone());
            coordinator.recovered_prepared = Some(batch.clone());
            match coordinator.issue_exact_recovered_authority(&batch)? {
                RecoveredBatchAuthority::Single(permit) => {
                    coordinator.commit_batch(&permit, &batch)?;
                }
                RecoveredBatchAuthority::ActiveTaskAdoption { actor } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::RecoveredActiveTaskAdoption { actor: &actor },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::TaskReconciliation { actor } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::RecoveredTaskReconciliation { actor: &actor },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ActorGoal { actor, goal } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ActorGoal {
                            actor: &actor,
                            goal: &goal,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ActorGoals {
                    actor,
                    first_goal,
                    second_goal,
                } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ActorGoals {
                            actor: &actor,
                            first_goal: &first_goal,
                            second_goal: &second_goal,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ActorGenerationTerminalization { actor, generation } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ActorGenerationTerminalization {
                            actor: &actor,
                            generation: &generation,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ActorGenerationInterrupt { actor, generation } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ActorGenerationInterrupt {
                            actor: &actor,
                            generation: &generation,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ActorFinalizerTaskTerminal { actor, finalizer } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ActorFinalizerTaskTerminal {
                            actor: &actor,
                            finalizer: &finalizer,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ActorBackgroundControl { actor, background } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ActorBackgroundControl {
                            actor: &actor,
                            background: &background,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ProviderBackgroundSuspend { actor, background } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ProviderBackgroundSuspend {
                            actor: &actor,
                            background: &background,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ProviderBackgroundInteractionRoute {
                    actor,
                    background,
                } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ProviderBackgroundInteractionRoute {
                            actor: &actor,
                            background: &background,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ProviderBackgroundInteractionResolution {
                    actor,
                    background,
                } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ProviderBackgroundInteractionResolution {
                            actor: &actor,
                            background: &background,
                            expected_safe_projection: None,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::ProviderBackgroundResume { actor, background } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::ProviderBackgroundResume {
                            actor: &actor,
                            background: &background,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::WorkflowBackgroundStop {
                    actor,
                    background,
                    finalizer,
                } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::WorkflowBackgroundStop {
                            actor: &actor,
                            background: &background,
                            finalizer: &finalizer,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::GoalGenerationStop {
                    finished_goal,
                    verification_goal,
                    decision_goal,
                    generation,
                    finalizer,
                } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::GoalGenerationStop {
                            finished_goal: &finished_goal,
                            verification_goal: verification_goal.as_ref(),
                            decision_goal: &decision_goal,
                            generation: &generation,
                            finalizer: &finalizer,
                        },
                        &batch,
                        None,
                    )?;
                }
                RecoveredBatchAuthority::GoalGenerationContinue {
                    actor,
                    finished_goal,
                    verification_goal,
                    decision_goal,
                    predecessor,
                } => {
                    coordinator.commit_batch_with_authority(
                        BatchCommitAuthority::GoalGenerationContinue {
                            actor: &actor,
                            finished_goal: &finished_goal,
                            verification_goal: verification_goal.as_ref(),
                            decision_goal: &decision_goal,
                            predecessor: &predecessor,
                        },
                        &batch,
                        None,
                    )?;
                }
            }
        }
        Ok(coordinator)
    }
}

impl RuntimeCommitCoordinator<'static, JsonlSurfaceCommitLedger> {
    pub fn recover_with_owned_lease(
        ledger: JsonlSurfaceCommitLedger,
        state: SurfaceReducerState,
        owner_lease: ExclusiveOwnerLease,
    ) -> Result<Self, SurfaceCommitError> {
        Self::recover_with_authority(ledger, state, OwnerLeaseAuthority::Owned(owner_lease))
    }
}

impl<'owner, L: SurfaceCommitLedger> RuntimeCommitCoordinator<'owner, L> {
    pub(crate) fn has_incomplete_batch(&self) -> bool {
        self.incomplete.is_some()
    }

    pub(crate) fn incomplete_batch_is(&self, batch: &SurfaceCommitBatch) -> bool {
        self.incomplete
            .as_ref()
            .is_some_and(|incomplete| incomplete == batch)
    }

    pub(crate) fn retry_incomplete_batch(
        &mut self,
    ) -> Result<Option<SurfaceCommitApplied>, SurfaceCommitError> {
        let Some(batch) = self.incomplete.clone() else {
            return Ok(None);
        };
        let authority = self.issue_exact_recovered_authority(&batch)?;
        let applied = match authority {
            RecoveredBatchAuthority::Single(permit) => self.commit_batch(&permit, &batch)?,
            RecoveredBatchAuthority::ActiveTaskAdoption { actor } => self
                .commit_batch_with_authority(
                    BatchCommitAuthority::RecoveredActiveTaskAdoption { actor: &actor },
                    &batch,
                    None,
                )?,
            RecoveredBatchAuthority::TaskReconciliation { actor } => self
                .commit_batch_with_authority(
                    BatchCommitAuthority::RecoveredTaskReconciliation { actor: &actor },
                    &batch,
                    None,
                )?,
            RecoveredBatchAuthority::ActorGoal { actor, goal } => self
                .commit_batch_with_authority(
                    BatchCommitAuthority::ActorGoal {
                        actor: &actor,
                        goal: &goal,
                    },
                    &batch,
                    None,
                )?,
            RecoveredBatchAuthority::ActorGoals {
                actor,
                first_goal,
                second_goal,
            } => self.commit_batch_with_authority(
                BatchCommitAuthority::ActorGoals {
                    actor: &actor,
                    first_goal: &first_goal,
                    second_goal: &second_goal,
                },
                &batch,
                None,
            )?,
            RecoveredBatchAuthority::ActorGenerationTerminalization { actor, generation } => self
                .commit_batch_with_authority(
                BatchCommitAuthority::ActorGenerationTerminalization {
                    actor: &actor,
                    generation: &generation,
                },
                &batch,
                None,
            )?,
            RecoveredBatchAuthority::ActorBackgroundControl { actor, background } => self
                .commit_batch_with_authority(
                    BatchCommitAuthority::ActorBackgroundControl {
                        actor: &actor,
                        background: &background,
                    },
                    &batch,
                    None,
                )?,
            RecoveredBatchAuthority::ActorGenerationInterrupt { actor, generation } => self
                .commit_batch_with_authority(
                    BatchCommitAuthority::ActorGenerationInterrupt {
                        actor: &actor,
                        generation: &generation,
                    },
                    &batch,
                    None,
                )?,
            RecoveredBatchAuthority::ActorFinalizerTaskTerminal { actor, finalizer } => self
                .commit_batch_with_authority(
                    BatchCommitAuthority::ActorFinalizerTaskTerminal {
                        actor: &actor,
                        finalizer: &finalizer,
                    },
                    &batch,
                    None,
                )?,
            RecoveredBatchAuthority::ProviderBackgroundSuspend { actor, background } => self
                .commit_batch_with_authority(
                    BatchCommitAuthority::ProviderBackgroundSuspend {
                        actor: &actor,
                        background: &background,
                    },
                    &batch,
                    None,
                )?,
            RecoveredBatchAuthority::ProviderBackgroundInteractionRoute { actor, background } => {
                self.commit_batch_with_authority(
                    BatchCommitAuthority::ProviderBackgroundInteractionRoute {
                        actor: &actor,
                        background: &background,
                    },
                    &batch,
                    None,
                )?
            }
            RecoveredBatchAuthority::ProviderBackgroundInteractionResolution {
                actor,
                background,
            } => self.commit_batch_with_authority(
                BatchCommitAuthority::ProviderBackgroundInteractionResolution {
                    actor: &actor,
                    background: &background,
                    expected_safe_projection: None,
                },
                &batch,
                None,
            )?,
            RecoveredBatchAuthority::ProviderBackgroundResume { actor, background } => self
                .commit_batch_with_authority(
                    BatchCommitAuthority::ProviderBackgroundResume {
                        actor: &actor,
                        background: &background,
                    },
                    &batch,
                    None,
                )?,
            RecoveredBatchAuthority::WorkflowBackgroundStop {
                actor,
                background,
                finalizer,
            } => self.commit_batch_with_authority(
                BatchCommitAuthority::WorkflowBackgroundStop {
                    actor: &actor,
                    background: &background,
                    finalizer: &finalizer,
                },
                &batch,
                None,
            )?,
            RecoveredBatchAuthority::GoalGenerationStop {
                finished_goal,
                verification_goal,
                decision_goal,
                generation,
                finalizer,
            } => self.commit_batch_with_authority(
                BatchCommitAuthority::GoalGenerationStop {
                    finished_goal: &finished_goal,
                    verification_goal: verification_goal.as_ref(),
                    decision_goal: &decision_goal,
                    generation: &generation,
                    finalizer: &finalizer,
                },
                &batch,
                None,
            )?,
            RecoveredBatchAuthority::GoalGenerationContinue {
                actor,
                finished_goal,
                verification_goal,
                decision_goal,
                predecessor,
            } => self.commit_batch_with_authority(
                BatchCommitAuthority::GoalGenerationContinue {
                    actor: &actor,
                    finished_goal: &finished_goal,
                    verification_goal: verification_goal.as_ref(),
                    decision_goal: &decision_goal,
                    predecessor: &predecessor,
                },
                &batch,
                None,
            )?,
        };
        Ok(Some(applied))
    }

    pub fn commit_actor_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch(&self.actor_control_permit.clone(), batch)
    }

    pub(crate) fn commit_terminal_task_reconciliation_batch(
        &mut self,
        receipt: &LegacyTerminalTaskReconciliationReceipt,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        self.commit_batch_with_authority(
            BatchCommitAuthority::TaskReconciliation {
                actor: &actor,
                receipt,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_active_task_adoption_batch(
        &mut self,
        receipt: &LegacyActiveTaskAdoptionReceipt,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        self.commit_batch_with_authority(
            BatchCommitAuthority::ActiveTaskAdoption {
                actor: &actor,
                receipt,
            },
            batch,
            None,
        )
    }

    pub fn commit_actor_batch_for_projection(
        &mut self,
        context: &SurfaceProjectionContext,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch_inner(&self.actor_control_permit.clone(), batch, Some(context))
    }

    pub fn commit_generation_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let permit = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch(&permit, batch)
    }

    /// Function intent contract:
    ///
    /// - Input: an unforgeable cold-owner authority bound to one historical
    ///   generation, one stable invocation id, its durable start receipt, and
    ///   the projection revision observed by the recovery owner.
    /// - Output: commits exactly the completed tool result and its paired
    ///   terminal item for that invocation.
    /// - Errors: rejects live-owner, stale-fence, wrong-invocation, stale
    ///   revision, missing-receipt, and non-terminal batch shapes before any
    ///   durable append.
    /// - State changes and external calls: appends one recorded surface batch;
    ///   it never creates an active operation and never invokes a provider or
    ///   tool.
    pub(crate) fn commit_historical_tool_result_batch(
        &mut self,
        authority: &HistoricalToolResultCommitAuthority,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch_with_authority(
            BatchCommitAuthority::HistoricalToolResult { authority },
            batch,
            None,
        )
    }

    /// Function intent contract:
    ///
    /// - Input: the exact stable tool-call identity and owning
    ///   operation/generation fence that are about to be externally invoked.
    /// - Output: a version-1 durable start receipt only after its recorded
    ///   surface event has been appended, checkpointed, and projected.
    /// - Errors: rejects missing, mismatched, already-started, executing, or
    ///   non-recorded invocations through the normal commit/reducer errors.
    /// - State changes and external calls: commits one durable surface patch;
    ///   it never invokes the tool. Callers must not perform any external tool
    ///   side effect unless this function returns `Ok`.
    pub fn commit_tool_invocation_started(
        &mut self,
        fence: super::SurfaceOperationFence,
        invocation_id: super::SurfaceToolCallId,
    ) -> Result<super::ToolInvocationStartedReceiptV1, SurfaceCommitError> {
        let receipt = super::ToolInvocationStartedReceiptV1::new(
            invocation_id,
            fence.clone(),
            super::ToolInvocationRevision::try_new(1)
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        );
        let background_fence = self
            .state
            .snapshot()
            .background_operations
            .iter()
            .find(|operation| operation.fence.operation_fence == fence)
            .map(|operation| operation.fence.clone());
        let scope = background_fence.as_ref().map_or_else(
            || SurfaceScope::Generation {
                fence: fence.clone(),
            },
            |background| SurfaceScope::Background {
                fence: background.clone(),
            },
        );
        let batch = self.tool_invocation_started_batch(scope, receipt.clone())?;
        if let Some(background_fence) = background_fence {
            self.commit_background_batch(background_fence, &batch)?;
        } else {
            self.commit_generation_batch(fence, &batch)?;
        }
        Ok(receipt)
    }

    pub(crate) fn commit_background_batch(
        &mut self,
        fence: super::SurfaceBackgroundFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let permit = self.register_permit(SurfacePublisherPermit::Background {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch(&permit, batch)
    }

    pub(crate) fn commit_goal_batch(
        &mut self,
        goal_fence: super::SurfaceGoalFence,
        receipt_digest: super::Sha256Digest,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let permit = self.register_permit(SurfacePublisherPermit::Goal {
            permit_id: next_permit_id(),
            goal_fence,
            receipt_digest,
        });
        self.commit_batch(&permit, batch)
    }

    pub(crate) fn commit_actor_goal_batch(
        &mut self,
        goal_fence: super::SurfaceGoalFence,
        receipt_digest: super::Sha256Digest,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let goal = self.register_permit(SurfacePublisherPermit::Goal {
            permit_id: next_permit_id(),
            goal_fence,
            receipt_digest,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ActorGoal {
                actor: &actor,
                goal: &goal,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_actor_two_goal_batch(
        &mut self,
        first_goal_fence: super::SurfaceGoalFence,
        first_receipt_digest: super::Sha256Digest,
        second_goal_fence: super::SurfaceGoalFence,
        second_receipt_digest: super::Sha256Digest,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let first_goal = self.register_permit(SurfacePublisherPermit::Goal {
            permit_id: next_permit_id(),
            goal_fence: first_goal_fence,
            receipt_digest: first_receipt_digest,
        });
        let second_goal = self.register_permit(SurfacePublisherPermit::Goal {
            permit_id: next_permit_id(),
            goal_fence: second_goal_fence,
            receipt_digest: second_receipt_digest,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ActorGoals {
                actor: &actor,
                first_goal: &first_goal,
                second_goal: &second_goal,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_actor_background_control_batch(
        &mut self,
        fence: super::SurfaceBackgroundFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let background = self.register_permit(SurfacePublisherPermit::Background {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ActorBackgroundControl {
                actor: &actor,
                background: &background,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_live_generation_stop_disposition_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let generation = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        let finalizer = self.issue_finalizer_permit(operation_id, finalize_intent_id);
        self.commit_batch_with_authority(
            BatchCommitAuthority::LiveGenerationStop {
                generation: &generation,
                finalizer: &finalizer,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_workflow_background_stop_batch(
        &mut self,
        fence: super::SurfaceBackgroundFence,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let background = self.register_permit(SurfacePublisherPermit::Background {
            permit_id: next_permit_id(),
            fence,
        });
        let finalizer = self.issue_finalizer_permit(operation_id, finalize_intent_id);
        self.commit_batch_with_authority(
            BatchCommitAuthority::WorkflowBackgroundStop {
                actor: &actor,
                background: &background,
                finalizer: &finalizer,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_provider_background_stop_batch(
        &mut self,
        fence: super::SurfaceBackgroundFence,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let background = self.register_permit(SurfacePublisherPermit::Background {
            permit_id: next_permit_id(),
            fence,
        });
        let finalizer = self.issue_finalizer_permit(operation_id, finalize_intent_id);
        self.commit_batch_with_authority(
            BatchCommitAuthority::WorkflowBackgroundStop {
                actor: &actor,
                background: &background,
                finalizer: &finalizer,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_provider_background_suspend_batch(
        &mut self,
        fence: super::SurfaceBackgroundFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let background = self.register_permit(SurfacePublisherPermit::Background {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ProviderBackgroundSuspend {
                actor: &actor,
                background: &background,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_provider_background_interaction_route_batch(
        &mut self,
        fence: super::SurfaceBackgroundFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let background = self.register_permit(SurfacePublisherPermit::Background {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ProviderBackgroundInteractionRoute {
                actor: &actor,
                background: &background,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_provider_background_interaction_resolution_batch(
        &mut self,
        fence: super::SurfaceBackgroundFence,
        expected_safe_projection: &super::SurfaceInteractionSafeProjection,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let background = self.register_permit(SurfacePublisherPermit::Background {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ProviderBackgroundInteractionResolution {
                actor: &actor,
                background: &background,
                expected_safe_projection: Some(expected_safe_projection),
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_provider_background_resume_batch(
        &mut self,
        fence: super::SurfaceBackgroundFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let background = self.register_permit(SurfacePublisherPermit::Background {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ProviderBackgroundResume {
                actor: &actor,
                background: &background,
            },
            batch,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_goal_generation_stop_batch(
        &mut self,
        finished_goal_fence: super::SurfaceGoalFence,
        finished_receipt_digest: super::Sha256Digest,
        verification: Option<(super::SurfaceGoalFence, super::Sha256Digest)>,
        decision_goal_fence: super::SurfaceGoalFence,
        decision_receipt_digest: super::Sha256Digest,
        fence: super::SurfaceOperationFence,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let finished_goal = self.register_permit(SurfacePublisherPermit::Goal {
            permit_id: next_permit_id(),
            goal_fence: finished_goal_fence,
            receipt_digest: finished_receipt_digest,
        });
        let decision_goal = self.register_permit(SurfacePublisherPermit::Goal {
            permit_id: next_permit_id(),
            goal_fence: decision_goal_fence,
            receipt_digest: decision_receipt_digest,
        });
        let verification_goal = verification.map(|(goal_fence, receipt_digest)| {
            self.register_permit(SurfacePublisherPermit::Goal {
                permit_id: next_permit_id(),
                goal_fence,
                receipt_digest,
            })
        });
        let generation = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        let finalizer = self.issue_finalizer_permit(operation_id, finalize_intent_id);
        self.commit_batch_with_authority(
            BatchCommitAuthority::GoalGenerationStop {
                finished_goal: &finished_goal,
                verification_goal: verification_goal.as_ref(),
                decision_goal: &decision_goal,
                generation: &generation,
                finalizer: &finalizer,
            },
            batch,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_goal_generation_continue_batch(
        &mut self,
        finished_goal_fence: super::SurfaceGoalFence,
        finished_receipt_digest: super::Sha256Digest,
        verification: Option<(super::SurfaceGoalFence, super::Sha256Digest)>,
        decision_goal_fence: super::SurfaceGoalFence,
        decision_receipt_digest: super::Sha256Digest,
        predecessor_fence: super::SurfaceOperationFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let finished_goal = self.register_permit(SurfacePublisherPermit::Goal {
            permit_id: next_permit_id(),
            goal_fence: finished_goal_fence,
            receipt_digest: finished_receipt_digest,
        });
        let verification_goal = verification.map(|(goal_fence, receipt_digest)| {
            self.register_permit(SurfacePublisherPermit::Goal {
                permit_id: next_permit_id(),
                goal_fence,
                receipt_digest,
            })
        });
        let decision_goal = self.register_permit(SurfacePublisherPermit::Goal {
            permit_id: next_permit_id(),
            goal_fence: decision_goal_fence,
            receipt_digest: decision_receipt_digest,
        });
        let predecessor = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence: predecessor_fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::GoalGenerationContinue {
                actor: &actor,
                finished_goal: &finished_goal,
                verification_goal: verification_goal.as_ref(),
                decision_goal: &decision_goal,
                predecessor: &predecessor,
            },
            batch,
            None,
        )
    }

    #[allow(dead_code)]
    pub(crate) fn commit_live_generation_suspend_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let generation = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::LiveGenerationSuspend {
                actor: &self.actor_control_permit.clone(),
                generation: &generation,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_resume_abort_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let recovery = self.issue_recovery_permit(fence);
        self.commit_batch_inner(&recovery, batch, None)
    }

    pub(crate) fn commit_actor_generation_terminalization_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let generation = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ActorGenerationTerminalization {
                actor: &actor,
                generation: &generation,
            },
            batch,
            None,
        )
    }

    pub(crate) fn commit_actor_generation_interrupt_batch(
        &mut self,
        fence: super::SurfaceOperationFence,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let generation = self.register_permit(SurfacePublisherPermit::Generation {
            permit_id: next_permit_id(),
            fence,
        });
        self.commit_batch_with_authority(
            BatchCommitAuthority::ActorGenerationInterrupt {
                actor: &actor,
                generation: &generation,
            },
            batch,
            None,
        )
    }

    pub fn commit_finalizer_batch(
        &mut self,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let permit = self.issue_finalizer_permit(operation_id, finalize_intent_id);
        self.commit_batch(&permit, batch)
    }

    pub(crate) fn commit_actor_finalizer_task_terminal_batch(
        &mut self,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        let finalizer = self.issue_finalizer_permit(operation_id, finalize_intent_id);
        self.commit_batch_with_authority(
            BatchCommitAuthority::ActorFinalizerTaskTerminal {
                actor: &actor,
                finalizer: &finalizer,
            },
            batch,
            None,
        )
    }

    pub fn ledger(&self) -> &L {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut L {
        &mut self.ledger
    }

    pub(crate) fn map_ledger<M>(
        self,
        map: impl FnOnce(L) -> M,
    ) -> RuntimeCommitCoordinator<'owner, M> {
        RuntimeCommitCoordinator {
            ledger: map(self.ledger),
            state: self.state,
            surface_hub: self.surface_hub,
            recovered_publications: self.recovered_publications,
            owner_lease: self.owner_lease,
            owner_epoch: self.owner_epoch,
            actor_control_permit: self.actor_control_permit,
            issued_permits: self.issued_permits,
            next_sequence: self.next_sequence,
            incomplete: self.incomplete,
            recovered_prepared: self.recovered_prepared,
            cold_takeover_authority: self.cold_takeover_authority,
            pending_projection: self.pending_projection,
            #[cfg(test)]
            projection_failure_injected: self.projection_failure_injected,
        }
    }

    pub fn state(&self) -> &SurfaceReducerState {
        &self.state
    }

    pub fn bind_surface_hub(
        &mut self,
        hub: super::SurfaceHub,
    ) -> Result<(), super::SurfaceHubBindError> {
        if self.surface_hub.is_some() {
            return Err(super::SurfaceHubBindError::AlreadyBound);
        }
        if hub.thread_id() != self.state.snapshot().thread.thread_id {
            return Err(super::SurfaceHubBindError::WrongThread);
        }
        let snapshot = std::sync::Arc::new(self.state.snapshot().clone());
        let publications = self.recovered_publications.make_contiguous();
        hub.repair_committed(snapshot, publications);
        self.surface_hub = Some(hub);
        self.recovered_publications.clear();
        Ok(())
    }

    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn recovery_action(
        &self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
    ) -> Option<RecoveryAction> {
        let snapshot = self.state.snapshot();
        let materialization_class = match materialization {
            super::MaterializationCause::SameProcessProjectionReset {
                retained_incarnation,
            } if retained_incarnation == &snapshot.cursor.incarnation => {
                RecoveryMaterialization::SameProcessProjectionReset
            }
            super::MaterializationCause::ColdOwnerTakeover {
                new_incarnation,
                new_owner_epoch,
            } if self
                .cold_takeover_authority
                .as_ref()
                .is_some_and(|authority| {
                    authority.authorizes(operation_id, snapshot, new_incarnation, new_owner_epoch)
                }) =>
            {
                RecoveryMaterialization::ColdOwnerTakeover
            }
            _ => return None,
        };
        let operation = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| &operation.operation_id == operation_id)?;
        let phase = match &operation.phase {
            super::OperationPhase::Requested => RecoverySourcePhase::Requested,
            super::OperationPhase::Admitted => {
                let generation = operation.generations.last()?;
                match generation.phase {
                    super::GenerationPhase::Reserved => RecoverySourcePhase::Reserved,
                    super::GenerationPhase::Started | super::GenerationPhase::Transferred => {
                        RecoverySourcePhase::StartedOrTransferred {
                            exact_terminal_interaction_unavailable: snapshot
                                .interactions
                                .iter()
                                .any(|interaction| {
                                    interaction.fence == generation.fence
                                        && (matches!(
                                            &interaction.lifecycle,
                                            super::SurfaceInteractionLifecycle::Expired { .. }
                                                | super::SurfaceInteractionLifecycle::Cancelled {
                                                    reason:
                                                        super::InteractionCancelReason::CapabilityUnavailable
                                                        | super::InteractionCancelReason::ExpiryAuthorityUnavailable { .. },
                                                }
                                        ) || (matches!(
                                            materialization_class,
                                            RecoveryMaterialization::ColdOwnerTakeover
                                        ) && matches!(
                                            interaction.lifecycle,
                                            super::SurfaceInteractionLifecycle::Resolved { .. }
                                        ) && matches!(
                                            interaction.kind,
                                            super::SurfaceInteractionKind::ToolApproval
                                                | super::SurfaceInteractionKind::PermissionRequest
                                                | super::SurfaceInteractionKind::UserInput
                                                | super::SurfaceInteractionKind::McpElicitation
                                        ) && matches!(
                                            interaction.recovery_disposition,
                                            super::InteractionUnavailableDisposition::FailOperation
                                        )))
                                }),
                        }
                    }
                    super::GenerationPhase::Stopped => return None,
                }
            }
            super::OperationPhase::Suspended { .. } => {
                let resume_starting = matches!(
                    (&operation.pending_control, operation.generations.last()),
                    (
                        Some(super::PendingControlIntent::ResumeStarting { generation_fence }),
                        Some(generation),
                    ) if generation.phase == super::GenerationPhase::Reserved
                        && generation_fence == &generation.fence
                );
                if resume_starting {
                    RecoverySourcePhase::ResumeStartingReserved
                } else {
                    RecoverySourcePhase::Suspended
                }
            }
            super::OperationPhase::Finalizing { .. } => RecoverySourcePhase::Finalizing,
            super::OperationPhase::FinalizingDegraded { .. } => {
                let cause = match self.state.finalization_degraded_cause(operation_id)? {
                    super::FinalizationDegradedCause::MissingFinalization { .. } => {
                        RecoveryDegradedCause::MissingFinalization
                    }
                    super::FinalizationDegradedCause::TerminalProjectionPending { .. } => {
                        RecoveryDegradedCause::TerminalProjectionPending
                    }
                };
                RecoverySourcePhase::FinalizingDegraded { cause }
            }
            super::OperationPhase::Terminal => RecoverySourcePhase::Terminal,
        };
        let replayability = match phase {
            RecoverySourcePhase::Requested
            | RecoverySourcePhase::StartedOrTransferred { .. }
            | RecoverySourcePhase::Finalizing
            | RecoverySourcePhase::FinalizingDegraded { .. }
            | RecoverySourcePhase::Terminal => RecoveryReplayability::NotApplicable,
            RecoverySourcePhase::Reserved
            | RecoverySourcePhase::Suspended
            | RecoverySourcePhase::ResumeStartingReserved => {
                let replayability = &operation.generations.last()?.replayability;
                match replayability {
                    super::Replayability::Replayable { .. } => RecoveryReplayability::Replayable,
                    super::Replayability::NonReplayable { live_capsule, .. } => {
                        let current = matches!(
                            (live_capsule, materialization_class),
                            (
                                super::LiveOperationCapsule::Available { incarnation },
                                RecoveryMaterialization::SameProcessProjectionReset,
                            ) if incarnation == &snapshot.cursor.incarnation
                        );
                        if current {
                            RecoveryReplayability::NonReplayableCurrent
                        } else {
                            RecoveryReplayability::NonReplayableNotCurrent
                        }
                    }
                }
            }
        };
        Some(decide_post_materialization_recovery(
            phase,
            replayability,
            materialization_class,
        ))
    }

    pub fn recover_operation(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
    ) -> Result<RecoveryAction, SurfaceCommitError> {
        self.recover_operation_inner(operation_id, materialization, None)
    }

    /// Function intent contract:
    ///
    /// - Input: one interrupted operation, its cold-owner materialization,
    ///   and a bounded safe diagnostic classifying why a persisted
    ///   interaction checkpoint cannot be restarted.
    /// - Output: begins the normal durable stop/finalize sequence with that
    ///   exact failure reason when the operation still owns a live generation;
    ///   otherwise preserves the ordinary recovery action.
    /// - Errors: rejects stale/missing operation state or any unauthorized
    ///   recovery batch before append.
    /// - State changes and external calls: may append generation-stop and
    ///   finalization facts; it never reconstructs a waiter, resumes a call
    ///   stack, dispatches a provider, or invokes a tool.
    pub(crate) fn recover_operation_checkpoint_rejection(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
        class: super::GenerationExecutionFailureClass,
        message: super::SafeDiagnosticText,
    ) -> Result<RecoveryAction, SurfaceCommitError> {
        let action = self
            .recovery_action(operation_id, materialization)
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        if !matches!(
            action,
            RecoveryAction::StopAndFinalizeRuntimeRestart
                | RecoveryAction::StopAndFinalizeClientCapabilityUnavailable
                | RecoveryAction::StopAndFinalizeRecoveryAbort
        ) {
            return self.recover_operation_inner(operation_id, materialization, None);
        }
        self.materialize_cold_owner_takeover(materialization)?;
        let operation = self
            .state
            .snapshot()
            .foreground_operation
            .iter()
            .chain(self.state.snapshot().queued_operations.iter())
            .chain(self.state.snapshot().operation_history.iter())
            .find(|operation| &operation.operation_id == operation_id)
            .cloned()
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let generation = self
            .recovery_generation(&operation)
            .cloned()
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let stop_reason = super::GenerationStopReason::ExecutionFailed { class, message };
        let mut batch = self.recovery_stop_and_finalize_batch(
            operation_id,
            generation.fence.clone(),
            stop_reason.clone(),
            super::OperationFinalizationCause::GenerationStop(stop_reason),
            None,
        )?;
        let pending_interactions = self
            .state
            .snapshot()
            .interactions
            .iter()
            .filter(|interaction| {
                interaction.fence == generation.fence
                    && matches!(
                        interaction.lifecycle,
                        super::SurfaceInteractionLifecycle::Requested
                    )
                    && matches!(
                        interaction.recovery_disposition,
                        super::InteractionUnavailableDisposition::FailOperation
                            | super::InteractionUnavailableDisposition::RestartableToolApproval {
                                ..
                            }
                            | super::InteractionUnavailableDisposition::RestartablePermissionRequest {
                                ..
                            }
                            | super::InteractionUnavailableDisposition::RestartableUserInput {
                                ..
                            }
                            | super::InteractionUnavailableDisposition::RestartableMcpElicitation {
                                ..
                            }
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        if !pending_interactions.is_empty() {
            let mut events = pending_interactions
                .into_iter()
                .map(
                    |interaction| -> Result<super::SurfaceEventEnvelope, SurfaceCommitError> {
                        let next_revision = super::InteractionRevision::try_new(
                            interaction
                                .revision
                                .get()
                                .checked_add(1)
                                .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                        )
                        .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                        Ok(super::SurfaceEventEnvelope {
                            ordinal: 0,
                            event_id: super::SurfaceEventId::try_from_bytes(
                                *uuid::Uuid::now_v7().as_bytes(),
                            )
                            .expect("generated UUID is v7"),
                            commit_class: batch.commit_class.clone(),
                            scope: SurfaceScope::Generation {
                                fence: interaction.fence,
                            },
                            event: super::SurfaceEvent::Interaction(
                                super::InteractionPatch::Cancelled {
                                    interaction_id: interaction.interaction_id,
                                    expected_revision: interaction.revision,
                                    next_revision,
                                    reason: super::InteractionCancelReason::CapabilityUnavailable,
                                },
                            ),
                        })
                    },
                )
                .collect::<Result<Vec<_>, _>>()?;
            events.extend(batch.events.as_slice().iter().cloned());
            if events.len() as u64 > super::SURFACE_COMMIT_BATCH_EVENT_LIMIT {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
            for (ordinal, event) in events.iter_mut().enumerate() {
                event.ordinal = ordinal as u32;
            }
            let event_count = u32::try_from(events.len())
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
            batch.cursor_after.next_seq = super::SequenceNumber::new(
                batch
                    .cursor_before
                    .next_seq
                    .get()
                    .checked_add(u64::from(event_count))
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
            );
            batch.event_count = event_count;
            batch.events = super::NonEmptyVec::try_new(events)
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
            batch.batch_digest = super::canonical_batch_digest(&batch);
        }
        let recovery_permit = self.issue_recovery_permit(generation.fence);
        self.commit_batch(&recovery_permit, &batch)?;
        Ok(action)
    }

    pub fn recover_unavailable_interactions(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
    ) -> Result<(), SurfaceCommitError> {
        self.recover_unavailable_interactions_except(operation_id, materialization, &[])
    }

    pub(crate) fn recover_unavailable_interactions_except(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
        retained_interactions: &[super::SurfaceInteractionId],
    ) -> Result<(), SurfaceCommitError> {
        if self
            .recovery_action(operation_id, materialization)
            .is_none()
        {
            return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
        }
        self.materialize_cold_owner_takeover(materialization)?;
        let interactions = self
            .state
            .snapshot()
            .interactions
            .iter()
            .filter(|interaction| {
                interaction.fence.operation_id == *operation_id
                    && !retained_interactions.contains(&interaction.interaction_id)
                    && matches!(
                        interaction.lifecycle,
                        super::SurfaceInteractionLifecycle::Requested
                    )
                    && matches!(
                        interaction.recovery_disposition,
                        super::InteractionUnavailableDisposition::FailOperation
                            | super::InteractionUnavailableDisposition::RestartableToolApproval { .. }
                            | super::InteractionUnavailableDisposition::RestartablePermissionRequest { .. }
                            | super::InteractionUnavailableDisposition::RestartableUserInput { .. }
                            | super::InteractionUnavailableDisposition::RestartableMcpElicitation { .. }
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        for interaction in interactions {
            let next_revision = super::InteractionRevision::try_new(
                interaction
                    .revision
                    .get()
                    .checked_add(1)
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
            )
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
            let batch = self.interaction_recovery_batch(
                interaction.fence.clone(),
                super::InteractionPatch::Cancelled {
                    interaction_id: interaction.interaction_id,
                    expected_revision: interaction.revision,
                    next_revision,
                    reason: super::InteractionCancelReason::CapabilityUnavailable,
                },
            )?;
            let permit = self.issue_recovery_permit(interaction.fence);
            self.commit_batch(&permit, &batch)?;
        }
        Ok(())
    }

    pub fn recover_interrupted_capability_calls(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
    ) -> Result<(), SurfaceCommitError> {
        if self
            .recovery_action(operation_id, materialization)
            .is_none()
        {
            return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
        }
        self.materialize_cold_owner_takeover(materialization)?;
        let calls = self
            .state
            .snapshot()
            .tools
            .iter()
            .flat_map(|tool| tool.capability_calls.iter())
            .filter(|call| {
                call.fence.operation_id == *operation_id
                    && matches!(
                        (call.kind, &call.state),
                        (
                            super::SurfaceCapabilityCallKind::ReadTextFile,
                            super::SurfaceCapabilityCallState::Prepared
                                | super::SurfaceCapabilityCallState::WrittenAwaitingResponse
                        ) | (
                            super::SurfaceCapabilityCallKind::TerminalOutput
                                | super::SurfaceCapabilityCallKind::TerminalWaitForExit,
                            super::SurfaceCapabilityCallState::Prepared
                                | super::SurfaceCapabilityCallState::WrittenAwaitingResponse
                        ) | (
                            super::SurfaceCapabilityCallKind::WriteTextFile,
                            super::SurfaceCapabilityCallState::Prepared
                                | super::SurfaceCapabilityCallState::DeliveryPossible
                                | super::SurfaceCapabilityCallState::WrittenAwaitingResponse
                        ) | (
                            super::SurfaceCapabilityCallKind::TerminalCreate,
                            super::SurfaceCapabilityCallState::Prepared
                                | super::SurfaceCapabilityCallState::DeliveryPossible
                                | super::SurfaceCapabilityCallState::WrittenAwaitingResponse
                        )
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        for mut call in calls {
            let message = match (call.kind, call.state) {
                (
                    super::SurfaceCapabilityCallKind::ReadTextFile
                    | super::SurfaceCapabilityCallKind::TerminalOutput
                    | super::SurfaceCapabilityCallKind::TerminalWaitForExit
                    | super::SurfaceCapabilityCallKind::WriteTextFile
                    | super::SurfaceCapabilityCallKind::TerminalCreate,
                    super::SurfaceCapabilityCallState::Prepared,
                ) => {
                    call.state = super::SurfaceCapabilityCallState::FailedBeforeWrite {
                        error: super::SafeDiagnosticText::try_new(
                            "runtime restarted before ACP capability request write",
                        )
                        .expect("fixed recovery diagnostic is bounded"),
                    };
                    "prepared"
                }
                (
                    super::SurfaceCapabilityCallKind::ReadTextFile
                    | super::SurfaceCapabilityCallKind::TerminalOutput
                    | super::SurfaceCapabilityCallKind::TerminalWaitForExit,
                    super::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                ) => {
                    call.state = super::SurfaceCapabilityCallState::ObservationUnavailable {
                        error: super::SafeDiagnosticText::try_new(
                            "runtime restarted before ACP capability response",
                        )
                        .expect("fixed recovery diagnostic is bounded"),
                    };
                    "written"
                }
                (
                    super::SurfaceCapabilityCallKind::WriteTextFile,
                    super::SurfaceCapabilityCallState::DeliveryPossible
                    | super::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                ) => {
                    call.state = super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                        effect_kind: super::ExternalEffectKind::FileWrite,
                        error: super::SafeDiagnosticText::try_new(
                            "runtime restarted after ACP file write delivery became possible",
                        )
                        .expect("fixed recovery diagnostic is bounded"),
                    };
                    "write-delivery-possible"
                }
                (
                    super::SurfaceCapabilityCallKind::TerminalCreate,
                    super::SurfaceCapabilityCallState::DeliveryPossible
                    | super::SurfaceCapabilityCallState::WrittenAwaitingResponse,
                ) => {
                    call.state = super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                        effect_kind: super::ExternalEffectKind::TerminalCreate,
                        error: super::SafeDiagnosticText::try_new(
                            "runtime restarted after ACP terminal create delivery became possible",
                        )
                        .expect("fixed recovery diagnostic is bounded"),
                    };
                    "terminal-create-delivery-possible"
                }
                _ => continue,
            };
            let fence = call.fence.clone();
            let batch = self.capability_recovery_batch(call)?;
            let permit = self.issue_recovery_permit(fence);
            self.commit_batch(&permit, &batch).map_err(|error| {
                eprintln!("orca: failed to settle {message} capability call: {error:?}");
                error
            })?;
        }
        let cleanup_leases = self
            .state
            .snapshot()
            .tools
            .iter()
            .flat_map(|tool| {
                tool.terminal_leases
                    .iter()
                    .filter_map(|lease| match &lease.state {
                        super::SurfaceRemoteTerminalLeaseState::Live {
                            terminal_id,
                            owner_fence,
                        }
                        | super::SurfaceRemoteTerminalLeaseState::KillPending {
                            terminal_id,
                            owner_fence,
                        }
                        | super::SurfaceRemoteTerminalLeaseState::ReleasePending {
                            terminal_id,
                            owner_fence,
                        } if owner_fence.operation_id == *operation_id => Some((
                            tool.request.tool_call_id.clone(),
                            lease.clone(),
                            terminal_id.clone(),
                            owner_fence.clone(),
                        )),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut cleanup_leases_by_tool = BTreeMap::new();
        for (tool_call_id, lease, terminal_id, fence) in cleanup_leases {
            cleanup_leases_by_tool
                .entry(tool_call_id)
                .or_insert_with(Vec::new)
                .push((lease, terminal_id, fence));
        }
        for (tool_call_id, cleanup_leases) in cleanup_leases_by_tool {
            let snapshot = self.state.snapshot().clone();
            let mut patches = Vec::new();
            let mut recovery_fence = None;
            for (lease, terminal_id, fence) in cleanup_leases {
                if recovery_fence
                    .as_ref()
                    .is_some_and(|existing| existing != &fence)
                {
                    return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
                }
                recovery_fence.get_or_insert_with(|| fence.clone());
                let terminal_digest = super::Sha256Digest::new(
                    sha2::Sha256::digest(terminal_id.as_str().as_bytes()).into(),
                );
                let existing = snapshot
                    .tools
                    .iter()
                    .find(|tool| tool.request.tool_call_id == tool_call_id)
                    .and_then(|tool| {
                        tool.capability_calls.iter().rev().find(|call| {
                            matches!(
                                call.kind,
                                super::SurfaceCapabilityCallKind::TerminalKill
                                    | super::SurfaceCapabilityCallKind::TerminalRelease
                            ) && call.fence == fence
                                && call.arguments_digest == terminal_digest
                                && !matches!(
                                call.state,
                                super::SurfaceCapabilityCallState::Completed { .. }
                                    | super::SurfaceCapabilityCallState::FailedBeforeWrite { .. }
                                    | super::SurfaceCapabilityCallState::ObservationUnavailable { .. }
                                    | super::SurfaceCapabilityCallState::ExternalEffectAmbiguous { .. }
                            )
                        })
                    })
                    .cloned();
                let existing_was_none = existing.is_none();
                let mut call = if let Some(call) = existing {
                    call
                } else {
                    let template = snapshot
                        .tools
                        .iter()
                        .find(|tool| tool.request.tool_call_id == tool_call_id)
                        .and_then(|tool| tool.capability_calls.last())
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                    let kind = match lease.state {
                        super::SurfaceRemoteTerminalLeaseState::ReleasePending { .. } => {
                            super::SurfaceCapabilityCallKind::TerminalRelease
                        }
                        _ => super::SurfaceCapabilityCallKind::TerminalKill,
                    };
                    super::SurfaceCapabilityCall {
                        call_id: super::SurfaceCapabilityCallId::try_from_bytes(
                            *uuid::Uuid::now_v7().as_bytes(),
                        )
                        .expect("generated UUID is v7"),
                        acp_session_id: template.acp_session_id.clone(),
                        fence: fence.clone(),
                        capability_revision: template.capability_revision,
                        policy_epoch: template.policy_epoch,
                        kind,
                        arguments_digest: terminal_digest,
                        owning_tool_call_id: tool_call_id.clone(),
                        state: super::SurfaceCapabilityCallState::Prepared,
                    }
                };
                if existing_was_none {
                    patches.push(super::SurfaceEvent::Tool(
                        super::ToolPatch::CapabilityCallChanged { call: call.clone() },
                    ));
                }
                if call.state == super::SurfaceCapabilityCallState::Prepared {
                    call.state = super::SurfaceCapabilityCallState::DeliveryPossible;
                    patches.push(super::SurfaceEvent::Tool(
                        super::ToolPatch::CapabilityCallChanged { call: call.clone() },
                    ));
                }
                let effect_kind = match call.kind {
                    super::SurfaceCapabilityCallKind::TerminalKill => {
                        super::ExternalEffectKind::TerminalKill
                    }
                    super::SurfaceCapabilityCallKind::TerminalRelease => {
                        super::ExternalEffectKind::TerminalRelease
                    }
                    _ => return Err(SurfaceCommitError::CursorRangeAlreadyConsumed),
                };
                call.state = super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                    effect_kind,
                    error: super::SafeDiagnosticText::try_new(
                        "runtime restarted before remote terminal cleanup completed",
                    )
                    .expect("fixed recovery diagnostic is bounded"),
                };
                patches.push(super::SurfaceEvent::Tool(
                    super::ToolPatch::CapabilityCallChanged { call: call.clone() },
                ));
                patches.push(super::SurfaceEvent::Tool(
                    super::ToolPatch::RemoteTerminalLeaseChanged {
                        lease: super::SurfaceRemoteTerminalLease {
                            lease_id: lease.lease_id,
                            owning_tool_call_id: tool_call_id.clone(),
                            state: super::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                                terminal_id: Some(terminal_id),
                                owner_fence: fence,
                            },
                        },
                    },
                ));
            }
            let fence = recovery_fence.ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
            let batch = self.terminal_cleanup_recovery_batch(fence.clone(), patches)?;
            let permit = self.issue_recovery_permit(fence);
            self.commit_batch(&permit, &batch)?;
        }
        let unsettled_ambiguous_tools = self
            .state
            .snapshot()
            .tools
            .iter()
            .filter(|tool| tool.result.is_none())
            .filter_map(|tool| {
                tool.capability_calls
                    .iter()
                    .find(|call| {
                        call.fence.operation_id == *operation_id
                            && matches!(
                                call.state,
                                super::SurfaceCapabilityCallState::ExternalEffectAmbiguous { .. }
                            )
                    })
                    .cloned()
            })
            .collect::<Vec<_>>();
        for call in unsettled_ambiguous_tools {
            let patches = self.ambiguous_capability_tool_recovery_patches(&call)?;
            let batch = self.terminal_cleanup_recovery_batch(call.fence.clone(), patches)?;
            let permit = self.issue_recovery_permit(call.fence);
            self.commit_batch(&permit, &batch)?;
        }
        Ok(())
    }

    pub(crate) fn recover_interrupted_manual_compaction(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        durable_snapshot: Option<&crate::thread_store::ManualCompactionDurableSnapshot>,
    ) -> Result<bool, SurfaceCommitError> {
        let snapshot = self.state.snapshot();
        let before_messages = match &snapshot.context.compaction {
            super::CompactionState::Running {
                operation_id: running,
                before_messages,
                ..
            } if running == operation_id => *before_messages,
            _ => return Ok(false),
        };
        let next_revision = super::ContextRevision::try_new(
            snapshot
                .context
                .revision
                .get()
                .checked_add(1)
                .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        )
        .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let mut context = snapshot.context.clone();
        context.revision = next_revision;
        if let Some(durable_snapshot) = durable_snapshot {
            if durable_snapshot.operation_id != *operation_id
                || durable_snapshot.before_messages as u64 != before_messages
            {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
            let observed_message_count = durable_snapshot.conversation.messages.len() as u64;
            let fence = snapshot
                .foreground_operation
                .iter()
                .chain(snapshot.queued_operations.iter())
                .chain(snapshot.operation_history.iter())
                .find(|operation| &operation.operation_id == operation_id)
                .and_then(|operation| operation.generations.last())
                .map(|generation| generation.fence.clone())
                .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
            context.compaction = super::CompactionState::Completed {
                operation_id: operation_id.clone(),
                reason: super::CompactionReason::Manual,
                strategy: super::NonEmptyText::try_new(durable_snapshot.strategy.clone())
                    .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                before_messages,
                after_messages: observed_message_count,
                collapsed_messages: before_messages.saturating_sub(observed_message_count),
                status_text: super::DisplayText::new("recovered completed manual compaction"),
            };
            let item_patches =
                manual_compaction_item_patches(&snapshot.items, &durable_snapshot.conversation)
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
            let batch =
                self.manual_compaction_recovery_batch(fence.clone(), item_patches, context)?;
            let permit = self.issue_recovery_permit(fence);
            self.commit_batch(&permit, &batch)?;
        } else {
            context.compaction = super::CompactionState::Idle;
            let batch = self.thread_context_recovery_batch(context)?;
            self.commit_actor_batch(&batch)?;
        }
        Ok(true)
    }

    pub fn recover_operation_with_settlement_store<S: super::ExternalSettlementStore>(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
        settlement_store: &mut S,
    ) -> Result<RecoveryAction, SurfaceCommitError> {
        self.recover_operation_inner(operation_id, materialization, Some(settlement_store))
    }

    fn recover_operation_inner(
        &mut self,
        operation_id: &super::SurfaceOperationId,
        materialization: &super::MaterializationCause,
        mut settlement_store: Option<&mut dyn super::ExternalSettlementStore>,
    ) -> Result<RecoveryAction, SurfaceCommitError> {
        let action = self
            .recovery_action(operation_id, materialization)
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        self.materialize_cold_owner_takeover(materialization)?;
        let operation = self
            .state
            .snapshot()
            .foreground_operation
            .iter()
            .chain(self.state.snapshot().queued_operations.iter())
            .chain(self.state.snapshot().operation_history.iter())
            .find(|operation| &operation.operation_id == operation_id)
            .cloned()
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        match action {
            RecoveryAction::FinalizeRequested => {
                let finalize_intent_id = super::SurfaceFinalizeIntentId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7");
                let terminal_commit_id =
                    super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                        .expect("generated UUID is v7");
                let finalizing = self.operation_recovery_batch(
                    operation_id,
                    super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                        .expect("generated UUID is v7"),
                    vec![super::OperationPatch::FinalizationStarted {
                        operation_id: operation_id.clone(),
                        finalize_intent_id: finalize_intent_id.clone(),
                        terminal_commit_id: terminal_commit_id.clone(),
                        selected_cause: super::OperationFinalizationCause::Reservation(
                            super::ReservationFinalizerReason::RuntimeRestart,
                        ),
                        suspended_cause: None,
                        expected_settlements: Vec::new(),
                    }],
                )?;
                let finalizer_permit =
                    self.issue_finalizer_permit(operation_id.clone(), finalize_intent_id.clone());
                self.commit_batch(&finalizer_permit, &finalizing)?;

                let terminal = self.operation_recovery_batch(
                    operation_id,
                    terminal_commit_id,
                    vec![super::OperationPatch::Terminal {
                        record: super::OperationTerminalRecord {
                            operation_id: operation_id.clone(),
                            finalize_intent_id,
                            terminal: super::OperationTerminal::NotAdmitted {
                                reason: super::NotAdmittedReason::RuntimeRestart,
                            },
                            usage: super::UsageTotals {
                                input_tokens: 0,
                                output_tokens: 0,
                                cache_tokens: 0,
                                estimated_cost_usd_micros: 0,
                            },
                            source_diagnostic_digest: None,
                            settlement_receipts: Vec::new(),
                            committed_at: super::UnixMillis::new(0),
                        },
                    }],
                )?;
                self.commit_batch(&finalizer_permit, &terminal)?;
                Ok(action)
            }
            RecoveryAction::StopAndSuspend => {
                let generation = self
                    .recovery_generation(&operation)
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let batch = self.operation_recovery_batch(
                    operation_id,
                    super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                        .expect("generated UUID is v7"),
                    vec![
                        super::OperationPatch::GenerationStopped {
                            fence: generation.fence.clone(),
                            reason: super::GenerationStopReason::NotStarted {
                                reason: super::NotStartedReason::RuntimeRestart,
                            },
                            usage_delta: super::UsageTotals {
                                input_tokens: 0,
                                output_tokens: 0,
                                cache_tokens: 0,
                                estimated_cost_usd_micros: 0,
                            },
                        },
                        super::OperationPatch::Suspended {
                            operation_id: operation_id.clone(),
                            cause: super::SuspensionCause::RecoveryRequired {
                                generation_id: generation.fence.generation_id,
                            },
                        },
                    ],
                )?;
                let recovery_permit = self.issue_recovery_permit(generation.fence);
                self.commit_batch(&recovery_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::StopAndFinalizeRuntimeRestart
            | RecoveryAction::StopAndFinalizeClientCapabilityUnavailable
            | RecoveryAction::StopAndFinalizeRecoveryAbort => {
                let generation = self
                    .recovery_generation(&operation)
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let has_external_effect_ambiguity =
                    self.state.snapshot().tools.iter().any(|tool| {
                        tool.capability_calls.iter().any(|call| {
                            call.fence == generation.fence
                                && matches!(
                                    &call.state,
                                    super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                        ..
                                    }
                                )
                        })
                    });
                let has_remote_cleanup_ambiguity = self.state.snapshot().tools.iter().any(|tool| {
                    tool.terminal_leases.iter().any(|lease| {
                        matches!(
                            lease.state,
                            super::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous { .. }
                        )
                    })
                });
                let stop_reason = match action {
                    RecoveryAction::StopAndFinalizeRuntimeRestart
                        if has_remote_cleanup_ambiguity =>
                    {
                        super::GenerationStopReason::ExecutionFailed {
                            class:
                                super::GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous,
                            message: super::SafeDiagnosticText::try_new(
                                "remote terminal cleanup is ambiguous after runtime restart",
                            )
                            .expect("static diagnostic is bounded"),
                        }
                    }
                    RecoveryAction::StopAndFinalizeRuntimeRestart
                        if has_external_effect_ambiguity =>
                    {
                        super::GenerationStopReason::ExecutionFailed {
                            class: super::GenerationExecutionFailureClass::ExternalEffectAmbiguous,
                            message: super::SafeDiagnosticText::try_new(
                                "external file write effect is ambiguous after runtime restart",
                            )
                            .expect("static diagnostic is bounded"),
                        }
                    }
                    RecoveryAction::StopAndFinalizeRuntimeRestart => {
                        super::GenerationStopReason::RuntimeRestart
                    }
                    RecoveryAction::StopAndFinalizeClientCapabilityUnavailable => {
                        super::GenerationStopReason::ExecutionFailed {
                            class:
                                super::GenerationExecutionFailureClass::ClientCapabilityUnavailable,
                            message: super::SafeDiagnosticText::try_new(
                                "required client capability became unavailable",
                            )
                            .expect("static diagnostic is bounded"),
                        }
                    }
                    RecoveryAction::StopAndFinalizeRecoveryAbort => {
                        super::GenerationStopReason::NotStarted {
                            reason: super::NotStartedReason::RuntimeRestart,
                        }
                    }
                    _ => unreachable!(),
                };
                let recovery_abort =
                    super::SuspendedFinalizationCause::RecoveryAbortNonReplayable {
                        last_generation: generation.fence.generation_id,
                    };
                let (selected_cause, suspended_cause) =
                    if matches!(operation.phase, super::OperationPhase::Suspended { .. }) {
                        (
                            super::OperationFinalizationCause::Suspended(recovery_abort.clone()),
                            Some(recovery_abort),
                        )
                    } else {
                        (
                            super::OperationFinalizationCause::GenerationStop(stop_reason.clone()),
                            None,
                        )
                    };
                let batch = self.recovery_stop_and_finalize_batch(
                    operation_id,
                    generation.fence.clone(),
                    stop_reason,
                    selected_cause,
                    suspended_cause,
                )?;
                let recovery_permit = self.issue_recovery_permit(generation.fence);
                self.commit_batch(&recovery_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::FinalizeRecoveryAbort => {
                let generation = self
                    .recovery_generation(&operation)
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let finalize_intent_id = super::SurfaceFinalizeIntentId::try_from_bytes(
                    *uuid::Uuid::now_v7().as_bytes(),
                )
                .expect("generated UUID is v7");
                let suspended_cause =
                    super::SuspendedFinalizationCause::RecoveryAbortNonReplayable {
                        last_generation: generation.fence.generation_id,
                    };
                let batch = self.recovery_finalization_batch(
                    operation_id,
                    finalize_intent_id.clone(),
                    super::OperationFinalizationCause::Suspended(suspended_cause.clone()),
                    Some(suspended_cause),
                )?;
                let finalizer_permit =
                    self.issue_finalizer_permit(operation_id.clone(), finalize_intent_id);
                self.commit_batch(&finalizer_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::StopAndRebaseSuspension => {
                let generation = self
                    .recovery_generation(&operation)
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let super::OperationPhase::Suspended { cause } = operation.phase else {
                    return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
                };
                let batch = self.operation_recovery_batch(
                    operation_id,
                    super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                        .expect("generated UUID is v7"),
                    vec![
                        super::OperationPatch::GenerationStopped {
                            fence: generation.fence.clone(),
                            reason: super::GenerationStopReason::NotStarted {
                                reason: super::NotStartedReason::RuntimeRestart,
                            },
                            usage_delta: zero_usage(),
                        },
                        super::OperationPatch::SuspensionRebasedAfterUnstartedResume {
                            operation_id: operation_id.clone(),
                            previous_cause: cause,
                            replacement_fence: generation.fence.clone(),
                            rebased_cause: super::SuspensionCause::RecoveryRequired {
                                generation_id: generation.fence.generation_id,
                            },
                        },
                    ],
                )?;
                let recovery_permit = self.issue_recovery_permit(generation.fence);
                self.commit_batch(&recovery_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::ReconcileOriginalFinalizer => {
                let finalization = operation
                    .finalization
                    .as_ref()
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                if !finalization.expected_settlements.is_empty() {
                    let store =
                        settlement_store
                            .as_deref_mut()
                            .ok_or(SurfaceCommitError::Settlement(
                                super::SettlementError::StoreUnavailable,
                            ))?;
                    let intent = super::DurableFinalizeIntent::new(
                        finalization.finalize_intent_id.clone(),
                        finalization.expected_settlements.clone(),
                    )
                    .map_err(SurfaceCommitError::Settlement)?;
                    let receipts = super::reconcile_finalize_intent(&intent, store)
                        .map_err(SurfaceCommitError::Settlement)?;
                    let missing = receipts
                        .into_iter()
                        .filter(|receipt| {
                            !finalization
                                .settled
                                .iter()
                                .any(|settled| settled.settlement_id == receipt.settlement_id)
                        })
                        .map(
                            |receipt| super::OperationPatch::FinalizationSettlementRecorded {
                                operation_id: operation_id.clone(),
                                finalize_intent_id: finalization.finalize_intent_id.clone(),
                                receipt,
                            },
                        )
                        .collect::<Vec<_>>();
                    if !missing.is_empty() {
                        let settlement_batch = self.operation_recovery_batch(
                            operation_id,
                            super::SurfaceCommitId::try_from_bytes(
                                *uuid::Uuid::now_v7().as_bytes(),
                            )
                            .expect("generated UUID is v7"),
                            missing,
                        )?;
                        let finalizer_permit = self.issue_finalizer_permit(
                            operation_id.clone(),
                            finalization.finalize_intent_id.clone(),
                        );
                        self.commit_batch(&finalizer_permit, &settlement_batch)?;
                    }
                }
                let operation = self
                    .state
                    .snapshot()
                    .foreground_operation
                    .iter()
                    .chain(self.state.snapshot().queued_operations.iter())
                    .chain(self.state.snapshot().operation_history.iter())
                    .find(|operation| &operation.operation_id == operation_id)
                    .cloned()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let finalization = operation
                    .finalization
                    .as_ref()
                    .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
                let usage = recovered_operation_usage(self.state.snapshot(), operation_id);
                let terminal = terminal_from_finalization(&operation, finalization, &usage)?;
                let batch = self.operation_recovery_batch(
                    operation_id,
                    finalization.terminal_commit_id.clone(),
                    vec![super::OperationPatch::Terminal {
                        record: super::OperationTerminalRecord {
                            operation_id: operation_id.clone(),
                            finalize_intent_id: finalization.finalize_intent_id.clone(),
                            terminal,
                            usage,
                            source_diagnostic_digest: None,
                            settlement_receipts: finalization.settled.clone(),
                            committed_at: super::UnixMillis::new(0),
                        },
                    }],
                )?;
                let finalizer_permit = self.issue_finalizer_permit(
                    operation_id.clone(),
                    finalization.finalize_intent_id.clone(),
                );
                self.commit_batch(&finalizer_permit, &batch)?;
                Ok(action)
            }
            RecoveryAction::ExposeRecoveryRequired
            | RecoveryAction::ExposeRetryFinalization
            | RecoveryAction::ExposeRetryProjection
            | RecoveryAction::NoOp => Ok(action),
        }
    }

    pub(crate) fn materialize_cold_owner_takeover(
        &mut self,
        materialization: &super::MaterializationCause,
    ) -> Result<(), SurfaceCommitError> {
        let super::MaterializationCause::ColdOwnerTakeover {
            new_incarnation,
            new_owner_epoch,
        } = materialization
        else {
            return Ok(());
        };
        let snapshot = self.state.snapshot();
        if snapshot.cursor.incarnation == *new_incarnation
            && snapshot.thread.owner_epoch == *new_owner_epoch
        {
            return if self
                .cold_takeover_authority
                .as_ref()
                .is_some_and(|authority| {
                    authority.authorizes_transition(snapshot, new_incarnation, new_owner_epoch)
                }) {
                Ok(())
            } else {
                Err(SurfaceCommitError::StaleOwnerEpoch)
            };
        }
        if !self
            .cold_takeover_authority
            .as_ref()
            .is_some_and(|authority| {
                authority.authorizes_transition(snapshot, new_incarnation, new_owner_epoch)
            })
            || new_owner_epoch != &self.owner_epoch
            || snapshot.thread.owner_epoch >= *new_owner_epoch
            || snapshot.cursor.incarnation == *new_incarnation
        {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }

        let cursor_before = snapshot.cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: *new_owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let event = super::SurfaceEventEnvelope {
            ordinal: 0,
            event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: super::SurfaceEvent::Session(super::SessionPatch::OwnerEpochChanged {
                previous: snapshot.thread.owner_epoch,
                next: *new_owner_epoch,
            }),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                incarnation: new_incarnation.clone(),
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count: 1,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(vec![event])
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        self.commit_actor_batch(&batch)?;
        if let Some(authority) = self.cold_takeover_authority.as_mut() {
            authority.new_incarnation = Some(new_incarnation.clone());
        }
        Ok(())
    }

    fn issue_finalizer_permit(
        &mut self,
        operation_id: super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
    ) -> SurfacePublisherPermit {
        self.register_permit(SurfacePublisherPermit::Finalizer {
            permit_id: next_permit_id(),
            operation_id,
            finalize_intent_id,
            owner_epoch: self.owner_epoch,
        })
    }

    fn issue_recovery_permit(
        &mut self,
        historical_fence: super::SurfaceOperationFence,
    ) -> SurfacePublisherPermit {
        self.register_permit(SurfacePublisherPermit::Recovery {
            permit_id: next_permit_id(),
            current_owner_epoch: self.owner_epoch,
            historical_fence,
        })
    }

    /// Function intent contract:
    ///
    /// - Input: the exact historical operation/generation fence, stable tool
    ///   invocation id, and projected invocation revision observed by a cold
    ///   recovery owner.
    /// - Output: an explicit commit authority only when the materialized cold
    ///   owner still owns that historical operation and the projected durable
    ///   `InvocationStarted` receipt matches every supplied identity.
    /// - Errors: stale owner/fence/revision, missing generation/tool/receipt,
    ///   or a non-running/non-completed tool projection are rejected without
    ///   changing durable state.
    /// - State changes and external calls: none; issuance is a read-only
    ///   authorization step and does not dispatch or commit anything.
    pub(crate) fn issue_historical_tool_result_commit_authority(
        &self,
        historical_fence: super::SurfaceOperationFence,
        invocation_id: super::SurfaceToolCallId,
        expected_projection_revision: super::ToolInvocationRevision,
    ) -> Result<HistoricalToolResultCommitAuthority, SurfaceCommitError> {
        let snapshot = self.state.snapshot();
        let cold_owner = self
            .cold_takeover_authority
            .as_ref()
            .ok_or(SurfaceCommitError::StalePublisherPermit)?;
        if !cold_owner.authorizes_historical_commit(
            &historical_fence,
            snapshot,
            &snapshot.cursor.incarnation,
            self.owner_epoch,
        ) {
            return Err(SurfaceCommitError::StalePublisherPermit);
        }
        let generation = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .find(|operation| operation.operation_id == historical_fence.operation_id)
            .and_then(|operation| {
                operation
                    .generations
                    .iter()
                    .find(|generation| generation.fence == historical_fence)
            })
            .ok_or(SurfaceCommitError::StalePublisherPermit)?;
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == invocation_id)
            .ok_or(SurfaceCommitError::StalePublisherPermit)?;
        let invocation_started = tool
            .invocation_started
            .as_ref()
            .filter(|receipt| {
                receipt.invocation_id() == &invocation_id
                    && receipt.fence() == &historical_fence
                    && receipt.revision() == expected_projection_revision
            })
            .cloned()
            .ok_or(SurfaceCommitError::StalePublisherPermit)?;
        if tool.request.turn_id != generation.logical_turn_id
            || !matches!(
                tool.state,
                super::SurfaceToolViewState::Running | super::SurfaceToolViewState::Completed
            )
        {
            return Err(SurfaceCommitError::StalePublisherPermit);
        }
        Ok(HistoricalToolResultCommitAuthority {
            current_owner_epoch: self.owner_epoch,
            current_incarnation: snapshot.cursor.incarnation.clone(),
            historical_fence,
            invocation_id,
            invocation_started,
            expected_projection_revision,
        })
    }

    fn register_permit(&mut self, permit: SurfacePublisherPermit) -> SurfacePublisherPermit {
        self.issued_permits.push(permit.clone());
        permit
    }

    fn issue_exact_recovered_authority(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<RecoveredBatchAuthority, SurfaceCommitError> {
        let actor = self.actor_control_permit.clone();
        if permit_authorizes(&self.issued_permits, &actor, batch, self.owner_epoch)
            && finalizer_background_scope_matches_state(&self.state, &actor, batch)
            && recovery_capability_completion_matches_state(&self.state, &actor, batch)
            && recovery_manual_compaction_matches_state(&self.state, &actor, batch)
        {
            return Ok(RecoveredBatchAuthority::Single(actor));
        }
        if recovered_terminal_task_reconciliation_authorized(
            &self.state,
            &self.issued_permits,
            &actor,
            batch,
            self.owner_epoch,
        ) {
            return Ok(RecoveredBatchAuthority::TaskReconciliation { actor });
        }
        if recovered_active_task_adoption_authorized(
            &self.state,
            &self.issued_permits,
            &actor,
            batch,
            self.owner_epoch,
        ) {
            return Ok(RecoveredBatchAuthority::ActiveTaskAdoption { actor });
        }

        let events = batch.events.as_slice();
        if let [task_event, terminal_event] = events
            && matches!(
                (&task_event.scope, &task_event.event),
                (
                    SurfaceScope::Thread,
                    super::SurfaceEvent::Task(super::TaskPatch::StatusChanged { .. })
                )
            )
            && let super::SurfaceEvent::Operation(super::OperationPatch::Terminal { record }) =
                &terminal_event.event
        {
            let finalizer = SurfacePublisherPermit::Finalizer {
                permit_id: next_permit_id(),
                operation_id: record.operation_id.clone(),
                finalize_intent_id: record.finalize_intent_id.clone(),
                owner_epoch: self.owner_epoch,
            };
            let mut issued = self.issued_permits.clone();
            issued.push(finalizer.clone());
            if actor_finalizer_task_terminal_authorized(
                &self.state,
                &issued,
                &actor,
                &finalizer,
                batch,
                self.owner_epoch,
            ) {
                return Ok(RecoveredBatchAuthority::ActorFinalizerTaskTerminal {
                    actor,
                    finalizer: self.register_permit(finalizer),
                });
            }
        }
        if let [control, ..] = events
            && let (
                SurfaceScope::Background { fence },
                super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
                    ..
                }),
            ) = (&control.scope, &control.event)
        {
            let background = SurfacePublisherPermit::Background {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            };
            let mut issued = self.issued_permits.clone();
            issued.push(background.clone());
            if actor_background_control_authorized(
                &self.state,
                &issued,
                &actor,
                &background,
                batch,
                self.owner_epoch,
            ) {
                return Ok(RecoveredBatchAuthority::ActorBackgroundControl {
                    actor,
                    background: self.register_permit(background),
                });
            }
        }
        if events.len() >= 4
            && let [.., stop, _, suspended] = events
            && let (
                SurfaceScope::Background { fence },
                super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                    reason: super::GenerationStopReason::ProviderSuspended,
                    ..
                }),
            ) = (&stop.scope, &stop.event)
            && matches!(
                &suspended.event,
                super::SurfaceEvent::Operation(super::OperationPatch::Suspended { .. })
            )
        {
            let background = SurfacePublisherPermit::Background {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            };
            let mut issued = self.issued_permits.clone();
            issued.push(background.clone());
            if provider_background_suspend_authorized(
                &self.state,
                &issued,
                &actor,
                &background,
                batch,
                self.owner_epoch,
            ) {
                return Ok(RecoveredBatchAuthority::ProviderBackgroundSuspend {
                    actor,
                    background: self.register_permit(background),
                });
            }
        }
        if let [route_event] = events
            && let (
                SurfaceScope::Background { fence },
                super::SurfaceEvent::Interaction(super::InteractionPatch::RouteChanged { .. }),
            ) = (&route_event.scope, &route_event.event)
        {
            let background = SurfacePublisherPermit::Background {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            };
            let mut issued = self.issued_permits.clone();
            issued.push(background.clone());
            if provider_background_interaction_route_authorized(
                &self.state,
                &issued,
                &actor,
                &background,
                batch,
                self.owner_epoch,
            ) {
                return Ok(
                    RecoveredBatchAuthority::ProviderBackgroundInteractionRoute {
                        actor,
                        background: self.register_permit(background),
                    },
                );
            }
        }
        if let [resolution_event] = events
            && let (
                SurfaceScope::Background { fence },
                super::SurfaceEvent::Interaction(super::InteractionPatch::Resolved { .. }),
            ) = (&resolution_event.scope, &resolution_event.event)
        {
            let background = SurfacePublisherPermit::Background {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            };
            let mut issued = self.issued_permits.clone();
            issued.push(background.clone());
            if provider_background_interaction_resolution_authorized(
                &self.state,
                &issued,
                &actor,
                &background,
                batch,
                self.owner_epoch,
                None,
            ) {
                return Ok(
                    RecoveredBatchAuthority::ProviderBackgroundInteractionResolution {
                        actor,
                        background: self.register_permit(background),
                    },
                );
            }
        }
        if let [reservation, _, _] = events
            && let (
                SurfaceScope::Background { fence },
                super::SurfaceEvent::Operation(super::OperationPatch::GenerationReserved {
                    ..
                }),
            ) = (&reservation.scope, &reservation.event)
        {
            let background = SurfacePublisherPermit::Background {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            };
            let mut issued = self.issued_permits.clone();
            issued.push(background.clone());
            if provider_background_resume_authorized(
                &self.state,
                &issued,
                &actor,
                &background,
                batch,
                self.owner_epoch,
            ) {
                return Ok(RecoveredBatchAuthority::ProviderBackgroundResume {
                    actor,
                    background: self.register_permit(background),
                });
            }
        }
        if events.len() >= 3
            && let [.., stop, finalization] = events
            && let (
                SurfaceScope::Background { fence },
                super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped { .. }),
            ) = (&stop.scope, &stop.event)
            && let super::SurfaceEvent::Operation(super::OperationPatch::FinalizationStarted {
                operation_id,
                finalize_intent_id,
                ..
            }) = &finalization.event
        {
            let background = SurfacePublisherPermit::Background {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            };
            let finalizer = SurfacePublisherPermit::Finalizer {
                permit_id: next_permit_id(),
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                owner_epoch: self.owner_epoch,
            };
            let mut issued = self.issued_permits.clone();
            issued.extend([background.clone(), finalizer.clone()]);
            if workflow_background_stop_authorized(
                &self.state,
                &issued,
                &actor,
                &background,
                &finalizer,
                batch,
                self.owner_epoch,
            ) {
                return Ok(RecoveredBatchAuthority::WorkflowBackgroundStop {
                    actor,
                    background: self.register_permit(background),
                    finalizer: self.register_permit(finalizer),
                });
            }
        }
        let admitted_goal_event_count = if events.len() >= 5
            && matches!(
                &events[events.len() - 3..],
                [finished, verification, decision]
                    if matches!(
                        &finished.event,
                        super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                            patch: super::GoalPatch::OuterTurnFinished { .. },
                            ..
                        })
                    ) && matches!(
                        &verification.event,
                        super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                            patch: super::GoalPatch::VerificationCompleted { .. },
                            ..
                        })
                    ) && matches!(
                        &decision.event,
                        super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                            patch: super::GoalPatch::ContinuationDecided {
                                decision: super::GoalContinuationDecision::Admitted { .. },
                                ..
                            },
                            ..
                        })
                    )
            ) {
            Some(3)
        } else if events.len() >= 4
            && matches!(
                &events[events.len() - 2..],
                [finished, decision]
                    if matches!(
                        &finished.event,
                        super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                            patch: super::GoalPatch::OuterTurnFinished { .. },
                            ..
                        })
                    ) && matches!(
                        &decision.event,
                        super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                            patch: super::GoalPatch::ContinuationDecided {
                                decision: super::GoalContinuationDecision::Admitted { .. },
                                ..
                            },
                            ..
                        })
                    )
            )
        {
            Some(2)
        } else {
            None
        };
        if let Some(goal_event_count) = admitted_goal_event_count {
            let goal_events = &events[events.len() - goal_event_count..];
            let receipts = goal_events
                .iter()
                .map(|event| match &event.event {
                    super::SurfaceEvent::Goal(envelope) => Some(envelope.receipt.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
                .expect("admitted Goal suffix contains Goal events");
            let (finished_receipt, verification_receipt, decision_receipt) =
                match receipts.as_slice() {
                    [finished, decision] => (finished, None, decision),
                    [finished, verification, decision] => (finished, Some(verification), decision),
                    _ => unreachable!("admitted Goal suffix is closed"),
                };
            let predecessor_fence = goal_events.last().and_then(|event| match &event.event {
                super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                    patch:
                        super::GoalPatch::ContinuationDecided {
                            predecessor,
                            decision: super::GoalContinuationDecision::Admitted { .. },
                            ..
                        },
                    ..
                }) => Some(predecessor.operation_fence.clone()),
                _ => None,
            });
            if let Some(predecessor_fence) = predecessor_fence {
                let goal_permit =
                    |receipt: &super::SurfaceGoalStoreReceipt| SurfacePublisherPermit::Goal {
                        permit_id: next_permit_id(),
                        goal_fence: super::SurfaceGoalFence {
                            goal_id: receipt.goal_id.clone(),
                            goal_revision: receipt.goal_revision,
                            goal_owner_epoch: receipt.goal_owner_epoch,
                        },
                        receipt_digest: receipt.receipt_digest.clone(),
                    };
                let finished_goal = goal_permit(finished_receipt);
                let verification_goal = verification_receipt.map(goal_permit);
                let decision_goal = goal_permit(decision_receipt);
                let predecessor = SurfacePublisherPermit::Generation {
                    permit_id: next_permit_id(),
                    fence: predecessor_fence,
                };
                let mut issued = self.issued_permits.clone();
                issued.push(finished_goal.clone());
                if let Some(verification_goal) = &verification_goal {
                    issued.push(verification_goal.clone());
                }
                issued.extend([decision_goal.clone(), predecessor.clone()]);
                if goal_generation_continue_authorized(
                    &self.state,
                    &issued,
                    &actor,
                    &finished_goal,
                    verification_goal.as_ref(),
                    &decision_goal,
                    &predecessor,
                    batch,
                    self.owner_epoch,
                ) {
                    return Ok(RecoveredBatchAuthority::GoalGenerationContinue {
                        actor,
                        finished_goal: self.register_permit(finished_goal),
                        verification_goal: verification_goal
                            .map(|permit| self.register_permit(permit)),
                        decision_goal: self.register_permit(decision_goal),
                        predecessor: self.register_permit(predecessor),
                    });
                }
            }
        }
        if events.len() >= 4 {
            let goal_event_count = if events.len() >= 5
                && matches!(
                    &events[events.len() - 3..],
                    [finished, verification, decision]
                        if matches!(
                            &finished.event,
                            super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                                patch: super::GoalPatch::OuterTurnFinished { .. },
                                ..
                            })
                        ) && matches!(
                            &verification.event,
                            super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                                patch: super::GoalPatch::VerificationCompleted { .. },
                                ..
                            })
                        ) && matches!(
                            &decision.event,
                            super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                                patch: super::GoalPatch::ContinuationDecided { .. },
                                ..
                            })
                        )
                ) {
                3
            } else {
                2
            };
            let goal_events = &events[events.len() - goal_event_count..];
            let goal_receipts = goal_events
                .iter()
                .map(|event| match &event.event {
                    super::SurfaceEvent::Goal(envelope) => Some(envelope.receipt.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            let core_events = &events[..events.len() - goal_event_count];
            let generation_fence = core_events.iter().find_map(|event| match &event.event {
                super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                    fence,
                    ..
                }) => Some(fence.clone()),
                _ => None,
            });
            let finalization = core_events.iter().find_map(|event| match &event.event {
                super::SurfaceEvent::Operation(super::OperationPatch::FinalizationStarted {
                    operation_id,
                    finalize_intent_id,
                    ..
                }) => Some((operation_id.clone(), finalize_intent_id.clone())),
                _ => None,
            });
            if let (
                Some(receipts),
                Some(generation_fence),
                Some((operation_id, finalize_intent_id)),
            ) = (goal_receipts, generation_fence, finalization)
            {
                let (finished_receipt, verification_receipt, decision_receipt) = match receipts
                    .as_slice()
                {
                    [finished, decision] => (finished, None, decision),
                    [finished, verification, decision] => (finished, Some(verification), decision),
                    _ => unreachable!("two or three trailing Goal events are required"),
                };
                let finished_goal = SurfacePublisherPermit::Goal {
                    permit_id: next_permit_id(),
                    goal_fence: super::SurfaceGoalFence {
                        goal_id: finished_receipt.goal_id.clone(),
                        goal_revision: finished_receipt.goal_revision,
                        goal_owner_epoch: finished_receipt.goal_owner_epoch,
                    },
                    receipt_digest: finished_receipt.receipt_digest.clone(),
                };
                let verification_goal =
                    verification_receipt.map(|receipt| SurfacePublisherPermit::Goal {
                        permit_id: next_permit_id(),
                        goal_fence: super::SurfaceGoalFence {
                            goal_id: receipt.goal_id.clone(),
                            goal_revision: receipt.goal_revision,
                            goal_owner_epoch: receipt.goal_owner_epoch,
                        },
                        receipt_digest: receipt.receipt_digest.clone(),
                    });
                let decision_goal = SurfacePublisherPermit::Goal {
                    permit_id: next_permit_id(),
                    goal_fence: super::SurfaceGoalFence {
                        goal_id: decision_receipt.goal_id.clone(),
                        goal_revision: decision_receipt.goal_revision,
                        goal_owner_epoch: decision_receipt.goal_owner_epoch,
                    },
                    receipt_digest: decision_receipt.receipt_digest.clone(),
                };
                let generation = SurfacePublisherPermit::Generation {
                    permit_id: next_permit_id(),
                    fence: generation_fence,
                };
                let finalizer = SurfacePublisherPermit::Finalizer {
                    permit_id: next_permit_id(),
                    operation_id,
                    finalize_intent_id,
                    owner_epoch: self.owner_epoch,
                };
                let mut issued = self.issued_permits.clone();
                issued.push(finished_goal.clone());
                if let Some(verification_goal) = &verification_goal {
                    issued.push(verification_goal.clone());
                }
                issued.extend([decision_goal.clone(), generation.clone(), finalizer.clone()]);
                if goal_generation_stop_authorized(
                    &self.state,
                    &issued,
                    &finished_goal,
                    verification_goal.as_ref(),
                    &decision_goal,
                    &generation,
                    &finalizer,
                    batch,
                    self.owner_epoch,
                ) {
                    return Ok(RecoveredBatchAuthority::GoalGenerationStop {
                        finished_goal: self.register_permit(finished_goal),
                        verification_goal: verification_goal
                            .map(|permit| self.register_permit(permit)),
                        decision_goal: self.register_permit(decision_goal),
                        generation: self.register_permit(generation),
                        finalizer: self.register_permit(finalizer),
                    });
                }
            }
        }
        if let [first, second, _] = events
            && let (
                super::SurfaceEvent::Goal(first_envelope),
                super::SurfaceEvent::Goal(second_envelope),
            ) = (&first.event, &second.event)
        {
            let first_goal = SurfacePublisherPermit::Goal {
                permit_id: next_permit_id(),
                goal_fence: super::SurfaceGoalFence {
                    goal_id: first_envelope.receipt.goal_id.clone(),
                    goal_revision: first_envelope.receipt.goal_revision,
                    goal_owner_epoch: first_envelope.receipt.goal_owner_epoch,
                },
                receipt_digest: first_envelope.receipt.receipt_digest.clone(),
            };
            let second_goal = SurfacePublisherPermit::Goal {
                permit_id: next_permit_id(),
                goal_fence: super::SurfaceGoalFence {
                    goal_id: second_envelope.receipt.goal_id.clone(),
                    goal_revision: second_envelope.receipt.goal_revision,
                    goal_owner_epoch: second_envelope.receipt.goal_owner_epoch,
                },
                receipt_digest: second_envelope.receipt.receipt_digest.clone(),
            };
            let mut issued = self.issued_permits.clone();
            issued.extend([first_goal.clone(), second_goal.clone()]);
            if actor_goal_edit_run_authorized(
                &issued,
                &actor,
                &first_goal,
                &second_goal,
                batch,
                self.owner_epoch,
            ) {
                return Ok(RecoveredBatchAuthority::ActorGoals {
                    actor,
                    first_goal: self.register_permit(first_goal),
                    second_goal: self.register_permit(second_goal),
                });
            }
        }
        if let Some(super::SurfaceEvent::Goal(super::GoalPatchEnvelope { receipt, .. })) =
            events.first().map(|event| &event.event)
        {
            let goal = SurfacePublisherPermit::Goal {
                permit_id: next_permit_id(),
                goal_fence: super::SurfaceGoalFence {
                    goal_id: receipt.goal_id.clone(),
                    goal_revision: receipt.goal_revision,
                    goal_owner_epoch: receipt.goal_owner_epoch,
                },
                receipt_digest: receipt.receipt_digest.clone(),
            };
            let mut issued = self.issued_permits.clone();
            issued.push(goal.clone());
            if actor_goal_run_start_authorized(&issued, &actor, &goal, batch, self.owner_epoch) {
                let goal = self.register_permit(goal);
                return Ok(RecoveredBatchAuthority::ActorGoal { actor, goal });
            }
        }
        if let Some(SurfaceScope::Generation {
            fence: historical_fence,
        }) = events.get(1).map(|event| &event.scope)
        {
            let generation = SurfacePublisherPermit::Generation {
                permit_id: next_permit_id(),
                fence: historical_fence.clone(),
            };
            let mut issued = self.issued_permits.clone();
            issued.push(generation.clone());
            if actor_generation_terminalization_authorized(
                &issued,
                &actor,
                &generation,
                batch,
                self.owner_epoch,
            ) {
                let generation = self.register_permit(generation);
                return Ok(RecoveredBatchAuthority::ActorGenerationTerminalization {
                    actor,
                    generation,
                });
            }
        }
        if let Some((historical_fence, operation_id)) =
            events
                .first()
                .and_then(|event| match (&event.scope, &event.event) {
                    (
                        SurfaceScope::Operation {
                            operation_id: scoped_operation_id,
                        },
                        super::SurfaceEvent::Operation(
                            super::OperationPatch::ControlIntentCommitted {
                                operation_id,
                                intent: super::PendingControlIntent::Interrupt { generation_fence },
                                ..
                            },
                        ),
                    ) if scoped_operation_id == operation_id
                        && operation_id == &generation_fence.operation_id =>
                    {
                        Some((generation_fence.clone(), operation_id.clone()))
                    }
                    _ => None,
                })
        {
            debug_assert_eq!(historical_fence.operation_id, operation_id);
            let generation = SurfacePublisherPermit::Generation {
                permit_id: next_permit_id(),
                fence: historical_fence,
            };
            let mut issued = self.issued_permits.clone();
            issued.push(generation.clone());
            if actor_generation_interrupt_authorized(
                &issued,
                &actor,
                &generation,
                batch,
                self.owner_epoch,
            ) {
                let generation = self.register_permit(generation);
                return Ok(RecoveredBatchAuthority::ActorGenerationInterrupt { actor, generation });
            }
        }
        let first = &events[0];
        let candidate = match (&first.scope, &first.event) {
            (
                _,
                super::SurfaceEvent::Operation(
                    super::OperationPatch::FinalizationStarted {
                        operation_id,
                        finalize_intent_id,
                        ..
                    }
                    | super::OperationPatch::FinalizationSettlementRecorded {
                        operation_id,
                        finalize_intent_id,
                        ..
                    }
                    | super::OperationPatch::FinalizationDegraded {
                        operation_id,
                        finalize_intent_id,
                        ..
                    }
                    | super::OperationPatch::Terminal {
                        record:
                            super::OperationTerminalRecord {
                                operation_id,
                                finalize_intent_id,
                                ..
                            },
                    },
                ),
            ) => SurfacePublisherPermit::Finalizer {
                permit_id: next_permit_id(),
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                owner_epoch: self.owner_epoch,
            },
            _ if events.iter().any(|event| {
                matches!(
                    &event.event,
                    super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped { .. })
                )
            }) =>
            {
                let historical_fence = events
                    .iter()
                    .find_map(|event| match &event.event {
                        super::SurfaceEvent::Operation(
                            super::OperationPatch::GenerationStopped { fence, .. },
                        ) => Some(fence.clone()),
                        _ => None,
                    })
                    .ok_or(SurfaceCommitError::StalePublisherPermit)?;
                SurfacePublisherPermit::Recovery {
                    permit_id: next_permit_id(),
                    current_owner_epoch: self.owner_epoch,
                    historical_fence,
                }
            }
            (SurfaceScope::Generation { fence }, _) => SurfacePublisherPermit::Generation {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            },
            (SurfaceScope::Background { fence }, _) => SurfacePublisherPermit::Background {
                permit_id: next_permit_id(),
                fence: fence.clone(),
            },
            (
                SurfaceScope::Goal { .. },
                super::SurfaceEvent::Goal(super::GoalPatchEnvelope { receipt, .. }),
            ) => SurfacePublisherPermit::Goal {
                permit_id: next_permit_id(),
                goal_fence: super::SurfaceGoalFence {
                    goal_id: receipt.goal_id.clone(),
                    goal_revision: receipt.goal_revision,
                    goal_owner_epoch: receipt.goal_owner_epoch,
                },
                receipt_digest: receipt.receipt_digest.clone(),
            },
            _ => return Err(SurfaceCommitError::StalePublisherPermit),
        };
        let mut issued = self.issued_permits.clone();
        issued.push(candidate.clone());
        if !permit_authorizes(&issued, &candidate, batch, self.owner_epoch)
            || !finalizer_background_scope_matches_state(&self.state, &candidate, batch)
            || !recovery_capability_completion_matches_state(&self.state, &candidate, batch)
            || !recovery_manual_compaction_matches_state(&self.state, &candidate, batch)
        {
            return Err(SurfaceCommitError::StalePublisherPermit);
        }
        Ok(RecoveredBatchAuthority::Single(
            self.register_permit(candidate),
        ))
    }

    fn recovery_generation<'a>(
        &self,
        operation: &'a super::OperationRecord,
    ) -> Option<&'a super::GenerationRecord> {
        operation.generations.last()
    }

    fn recovery_stop_and_finalize_batch(
        &self,
        operation_id: &super::SurfaceOperationId,
        fence: super::SurfaceOperationFence,
        stop_reason: super::GenerationStopReason,
        selected_cause: super::OperationFinalizationCause,
        suspended_cause: Option<super::SuspendedFinalizationCause>,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let discard_reason = match &stop_reason {
            super::GenerationStopReason::Cancelled { .. } => {
                super::AssistantDiscardReason::GenerationCancelled
            }
            super::GenerationStopReason::InterruptedResumable => {
                super::AssistantDiscardReason::GenerationInterrupted
            }
            super::GenerationStopReason::RuntimeRestart
            | super::GenerationStopReason::NotStarted {
                reason: super::NotStartedReason::RuntimeRestart,
            } => super::AssistantDiscardReason::RuntimeRestart,
            super::GenerationStopReason::ProjectionFailure { .. } => {
                super::AssistantDiscardReason::ProjectionRepair
            }
            _ => super::AssistantDiscardReason::ProviderFailed,
        };
        let open_streams = self
            .state
            .snapshot()
            .assistant_streams
            .iter()
            .filter(|stream| {
                stream.fence == fence && stream.state == super::SurfaceAssistantStreamState::Open
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut patches = vec![super::OperationPatch::GenerationStopped {
            fence: fence.clone(),
            reason: stop_reason,
            usage_delta: zero_usage(),
        }];
        let finalization =
            self.recovery_finalization_patch(operation_id, selected_cause, suspended_cause);
        patches.push(finalization);
        let mut batch = self.operation_recovery_batch(
            operation_id,
            super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            patches,
        )?;
        if open_streams.is_empty() {
            return Ok(batch);
        }

        let background_scope = self
            .state
            .snapshot()
            .background_operations
            .iter()
            .find(|operation| &operation.operation_id == operation_id)
            .map(|operation| SurfaceScope::Background {
                fence: operation.fence.clone(),
            });
        let stream_scope = background_scope.unwrap_or_else(|| SurfaceScope::Generation { fence });
        let mut events = open_streams
            .into_iter()
            .map(|stream| super::SurfaceEventEnvelope {
                ordinal: 0,
                event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7"),
                commit_class: batch.commit_class.clone(),
                scope: stream_scope.clone(),
                event: super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded {
                    stream_id: stream.stream_id,
                    reason: discard_reason,
                }),
            })
            .collect::<Vec<_>>();
        events.extend(batch.events.as_slice().iter().cloned());
        for (ordinal, event) in events.iter_mut().enumerate() {
            event.ordinal = ordinal as u32;
        }
        batch.event_count = u32::try_from(events.len())
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        batch.cursor_after.next_seq = super::SequenceNumber::new(
            batch
                .cursor_before
                .next_seq
                .get()
                .checked_add(batch.event_count as u64)
                .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        );
        batch.events = super::NonEmptyVec::try_new(events)
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn recovery_finalization_batch(
        &self,
        operation_id: &super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        selected_cause: super::OperationFinalizationCause,
        suspended_cause: Option<super::SuspendedFinalizationCause>,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        self.operation_recovery_batch(
            operation_id,
            super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            vec![self.recovery_finalization_patch_with_intent(
                operation_id,
                finalize_intent_id,
                selected_cause,
                suspended_cause,
            )],
        )
    }

    fn recovery_finalization_patch(
        &self,
        operation_id: &super::SurfaceOperationId,
        selected_cause: super::OperationFinalizationCause,
        suspended_cause: Option<super::SuspendedFinalizationCause>,
    ) -> super::OperationPatch {
        self.recovery_finalization_patch_with_intent(
            operation_id,
            super::SurfaceFinalizeIntentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            selected_cause,
            suspended_cause,
        )
    }

    fn recovery_finalization_patch_with_intent(
        &self,
        operation_id: &super::SurfaceOperationId,
        finalize_intent_id: super::SurfaceFinalizeIntentId,
        selected_cause: super::OperationFinalizationCause,
        suspended_cause: Option<super::SuspendedFinalizationCause>,
    ) -> super::OperationPatch {
        super::OperationPatch::FinalizationStarted {
            operation_id: operation_id.clone(),
            finalize_intent_id,
            terminal_commit_id: super::SurfaceCommitId::try_from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            )
            .expect("generated UUID is v7"),
            selected_cause,
            suspended_cause,
            expected_settlements: Vec::new(),
        }
    }

    fn operation_recovery_batch(
        &self,
        operation_id: &super::SurfaceOperationId,
        commit_id: super::SurfaceCommitId,
        patches: Vec<super::OperationPatch>,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id,
        };
        let events = patches
            .into_iter()
            .enumerate()
            .map(|(ordinal, patch)| {
                let background_scope = self
                    .state
                    .snapshot()
                    .background_operations
                    .iter()
                    .find(|operation| &operation.operation_id == operation_id)
                    .map(|operation| SurfaceScope::Background {
                        fence: operation.fence.clone(),
                    });
                let scope = match &patch {
                    super::OperationPatch::GenerationStopped { fence, .. } => background_scope
                        .unwrap_or_else(|| SurfaceScope::Generation {
                            fence: fence.clone(),
                        }),
                    _ => background_scope.unwrap_or_else(|| SurfaceScope::Operation {
                        operation_id: operation_id.clone(),
                    }),
                };
                super::SurfaceEventEnvelope {
                    ordinal: ordinal as u32,
                    event_id: super::SurfaceEventId::try_from_bytes(
                        *uuid::Uuid::now_v7().as_bytes(),
                    )
                    .expect("generated UUID is v7"),
                    commit_class: commit_class.clone(),
                    scope,
                    event: super::SurfaceEvent::Operation(patch),
                }
            })
            .collect::<Vec<_>>();
        let event_count = u32::try_from(events.len())
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let events = super::NonEmptyVec::try_new(events)
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(event_count as u64)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn thread_context_recovery_batch(
        &self,
        context: super::SurfaceContextSnapshot,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let event = super::SurfaceEventEnvelope {
            ordinal: 0,
            event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: super::SurfaceEvent::Context(context),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count: 1,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(vec![event])
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn tool_invocation_started_batch(
        &self,
        scope: SurfaceScope,
        receipt: super::ToolInvocationStartedReceiptV1,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let event = super::SurfaceEventEnvelope {
            ordinal: 0,
            event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            commit_class: commit_class.clone(),
            scope,
            event: super::SurfaceEvent::Tool(super::ToolPatch::InvocationStartedV1 { receipt }),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count: 1,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(vec![event])
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn manual_compaction_recovery_batch(
        &self,
        fence: super::SurfaceOperationFence,
        item_patches: Vec<super::ItemPatch>,
        context: super::SurfaceContextSnapshot,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let scope = SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let mut facts = item_patches
            .into_iter()
            .map(|patch| (scope.clone(), super::SurfaceEvent::Item(patch)))
            .collect::<Vec<_>>();
        facts.push((scope, super::SurfaceEvent::Context(context)));
        if facts.len() as u64 > super::SURFACE_COMMIT_BATCH_EVENT_LIMIT {
            return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
        }
        let event_count = u32::try_from(facts.len())
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let events = facts
            .into_iter()
            .enumerate()
            .map(|(ordinal, (scope, event))| super::SurfaceEventEnvelope {
                ordinal: ordinal as u32,
                event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7"),
                commit_class: commit_class.clone(),
                scope,
                event,
            })
            .collect::<Vec<_>>();
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(event_count as u64)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(events)
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn interaction_recovery_batch(
        &self,
        fence: super::SurfaceOperationFence,
        patch: super::InteractionPatch,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let event = super::SurfaceEventEnvelope {
            ordinal: 0,
            event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Generation { fence },
            event: super::SurfaceEvent::Interaction(patch),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count: 1,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(vec![event])
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn capability_recovery_batch(
        &self,
        call: super::SurfaceCapabilityCall,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let scope = SurfaceScope::Generation {
            fence: call.fence.clone(),
        };
        let mut patches = vec![super::SurfaceEvent::Tool(
            super::ToolPatch::CapabilityCallChanged { call: call.clone() },
        )];
        if let super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
            effect_kind:
                super::ExternalEffectKind::FileWrite | super::ExternalEffectKind::TerminalCreate,
            ..
        } = &call.state
        {
            if matches!(
                call.state,
                super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                    effect_kind: super::ExternalEffectKind::TerminalCreate,
                    ..
                }
            ) {
                patches.push(super::SurfaceEvent::Tool(
                    super::ToolPatch::RemoteTerminalLeaseChanged {
                        lease: super::SurfaceRemoteTerminalLease {
                            lease_id: super::UuidV7::try_from_bytes(*call.call_id.as_bytes())
                                .expect("capability call id is a UUIDv7"),
                            owning_tool_call_id: call.owning_tool_call_id.clone(),
                            state: super::SurfaceRemoteTerminalLeaseState::IdentityUnknown {
                                create_call_id: call.call_id.clone(),
                            },
                        },
                    },
                ));
            }
        }
        let events = patches
            .into_iter()
            .enumerate()
            .map(|(ordinal, event)| super::SurfaceEventEnvelope {
                ordinal: ordinal as u32,
                event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7"),
                commit_class: commit_class.clone(),
                scope: scope.clone(),
                event,
            })
            .collect::<Vec<_>>();
        let event_count = u32::try_from(events.len())
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(u64::from(event_count))
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(events)
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    fn ambiguous_capability_tool_recovery_patches(
        &self,
        call: &super::SurfaceCapabilityCall,
    ) -> Result<Vec<super::SurfaceEvent>, SurfaceCommitError> {
        let super::SurfaceCapabilityCallState::ExternalEffectAmbiguous { error, .. } = &call.state
        else {
            return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
        };
        let tool = self
            .state
            .snapshot()
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id == call.owning_tool_call_id)
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        if tool.result.is_some() {
            return Ok(Vec::new());
        }
        let terminal = super::SurfaceToolTerminal {
            kind: super::SurfaceToolResultKind::ExternalEffectAmbiguous,
            source: super::ToolTerminalSource::Observed,
            invocation_started: super::ToolInvocationStarted::Yes,
        };
        let content = super::DisplayText::new(error.as_str());
        Ok(vec![
            super::SurfaceEvent::Tool(super::ToolPatch::Completed {
                result: super::SurfaceToolResult {
                    tool_call_id: call.owning_tool_call_id.clone(),
                    name: tool.request.name.clone(),
                    terminal: terminal.clone(),
                    output: None,
                    error: Some(content.clone()),
                    exit_code: None,
                    truncated: false,
                    file_change: None,
                },
            }),
            super::SurfaceEvent::Item(super::ItemPatch::Added {
                item: super::SurfaceItem::ToolResultMessage {
                    id: super::SurfaceItemId::new(),
                    turn_id: tool.request.turn_id.clone(),
                    tool_call_id: tool.request.tool_call_id.clone(),
                    content,
                    terminal,
                    pinned: false,
                },
            }),
        ])
    }

    fn terminal_cleanup_recovery_batch(
        &self,
        fence: super::SurfaceOperationFence,
        patches: Vec<super::SurfaceEvent>,
    ) -> Result<SurfaceCommitBatch, SurfaceCommitError> {
        let cursor_before = self.state.snapshot().cursor.clone();
        let durable_revision = match cursor_before.source_revision {
            super::CursorSourceRevision::Recorded { durable_revision } => {
                super::DurableRevision::try_new(
                    durable_revision
                        .get()
                        .checked_add(1)
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                )
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?
            }
            super::CursorSourceRevision::Ephemeral { .. } => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
        };
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: self.owner_epoch,
            durable_revision,
            commit_id: super::SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .expect("generated UUID is v7"),
        };
        let scope = SurfaceScope::Generation { fence };
        let events = patches
            .into_iter()
            .enumerate()
            .map(|(ordinal, event)| super::SurfaceEventEnvelope {
                ordinal: ordinal as u32,
                event_id: super::SurfaceEventId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
                    .expect("generated UUID is v7"),
                commit_class: commit_class.clone(),
                scope: scope.clone(),
                event,
            })
            .collect::<Vec<_>>();
        let event_count = u32::try_from(events.len())
            .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?;
        let mut batch = SurfaceCommitBatch {
            cursor_before: cursor_before.clone(),
            cursor_after: super::SurfaceCursor {
                next_seq: super::SequenceNumber::new(
                    cursor_before
                        .next_seq
                        .get()
                        .checked_add(u64::from(event_count))
                        .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)?,
                ),
                source_revision: super::CursorSourceRevision::Recorded { durable_revision },
                ..cursor_before
            },
            commit_class,
            event_count,
            batch_digest: super::Sha256Digest::new([0; 32]),
            events: super::NonEmptyVec::try_new(events)
                .map_err(|_| SurfaceCommitError::CursorRangeAlreadyConsumed)?,
        };
        batch.batch_digest = super::canonical_batch_digest(&batch);
        Ok(batch)
    }

    pub fn commit_batch(
        &mut self,
        permit: &SurfacePublisherPermit,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch_inner(permit, batch, None)
    }

    pub fn commit_batch_for_projection(
        &mut self,
        permit: &SurfacePublisherPermit,
        context: &SurfaceProjectionContext,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch_inner(permit, batch, Some(context))
    }

    fn commit_batch_inner(
        &mut self,
        permit: &SurfacePublisherPermit,
        batch: &SurfaceCommitBatch,
        projection_context: Option<&SurfaceProjectionContext>,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        self.commit_batch_with_authority(
            BatchCommitAuthority::Single(permit),
            batch,
            projection_context,
        )
    }

    fn commit_batch_with_authority(
        &mut self,
        authority: BatchCommitAuthority<'_>,
        batch: &SurfaceCommitBatch,
        projection_context: Option<&SurfaceProjectionContext>,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        let extends_cold_recovery = matches!(
            &authority,
            BatchCommitAuthority::ActiveTaskAdoption { .. }
                | BatchCommitAuthority::RecoveredActiveTaskAdoption { .. }
        );
        if !self
            .owner_lease
            .lease()
            .authorizes_thread(&self.state.snapshot().thread.thread_id)
        {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        if let Some((token, pending_batch)) = &self.pending_projection {
            return if prepared_identity(pending_batch) == prepared_identity(batch) {
                Err(SurfaceCommitError::ProjectionPending {
                    token: token.clone(),
                })
            } else {
                Err(SurfaceCommitError::CursorRangeAlreadyConsumed)
            };
        }
        let authorized = match authority {
            BatchCommitAuthority::Single(permit) => {
                permit_authorizes(&self.issued_permits, permit, batch, self.owner_epoch)
                    && finalizer_background_scope_matches_state(&self.state, permit, batch)
                    && recovery_stream_dispositions_match_state(&self.state, permit, batch)
                    && recovery_capability_completion_matches_state(&self.state, permit, batch)
                    && recovery_manual_compaction_matches_state(&self.state, permit, batch)
            }
            BatchCommitAuthority::ActiveTaskAdoption { actor, receipt } => {
                active_task_adoption_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    receipt,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::RecoveredActiveTaskAdoption { actor } => {
                recovered_active_task_adoption_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::TaskReconciliation { actor, receipt } => {
                terminal_task_reconciliation_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    receipt,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::RecoveredTaskReconciliation { actor } => {
                recovered_terminal_task_reconciliation_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::ActorGoal { actor, goal } => actor_goal_run_start_authorized(
                &self.issued_permits,
                actor,
                goal,
                batch,
                self.owner_epoch,
            ),
            BatchCommitAuthority::ActorGoals {
                actor,
                first_goal,
                second_goal,
            } => actor_goal_edit_run_authorized(
                &self.issued_permits,
                actor,
                first_goal,
                second_goal,
                batch,
                self.owner_epoch,
            ),
            BatchCommitAuthority::ActorGenerationTerminalization { actor, generation } => {
                actor_generation_terminalization_authorized(
                    &self.issued_permits,
                    actor,
                    generation,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::ActorGenerationInterrupt { actor, generation } => {
                actor_generation_interrupt_authorized(
                    &self.issued_permits,
                    actor,
                    generation,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::ActorFinalizerTaskTerminal { actor, finalizer } => {
                actor_finalizer_task_terminal_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    finalizer,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::ActorBackgroundControl { actor, background } => {
                actor_background_control_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    background,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::ProviderBackgroundSuspend { actor, background } => {
                provider_background_suspend_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    background,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::ProviderBackgroundInteractionRoute { actor, background } => {
                provider_background_interaction_route_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    background,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::ProviderBackgroundInteractionResolution {
                actor,
                background,
                expected_safe_projection,
            } => provider_background_interaction_resolution_authorized(
                &self.state,
                &self.issued_permits,
                actor,
                background,
                batch,
                self.owner_epoch,
                expected_safe_projection,
            ),
            BatchCommitAuthority::ProviderBackgroundResume { actor, background } => {
                provider_background_resume_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    background,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::WorkflowBackgroundStop {
                actor,
                background,
                finalizer,
            } => workflow_background_stop_authorized(
                &self.state,
                &self.issued_permits,
                actor,
                background,
                finalizer,
                batch,
                self.owner_epoch,
            ),
            BatchCommitAuthority::LiveGenerationSuspend { actor, generation } => {
                live_generation_suspend_authorized(
                    &self.state,
                    &self.issued_permits,
                    actor,
                    generation,
                    batch,
                    self.owner_epoch,
                )
            }
            BatchCommitAuthority::LiveGenerationStop {
                generation,
                finalizer,
            } => {
                live_generation_stop_disposition_authorized(
                    &self.state,
                    &self.issued_permits,
                    generation,
                    finalizer,
                    batch,
                    self.owner_epoch,
                ) && finalizer_background_scope_matches_state(&self.state, finalizer, batch)
            }
            BatchCommitAuthority::GoalGenerationStop {
                finished_goal,
                verification_goal,
                decision_goal,
                generation,
                finalizer,
            } => {
                goal_generation_stop_authorized(
                    &self.state,
                    &self.issued_permits,
                    finished_goal,
                    verification_goal,
                    decision_goal,
                    generation,
                    finalizer,
                    batch,
                    self.owner_epoch,
                ) && finalizer_background_scope_matches_state(&self.state, finalizer, batch)
            }
            BatchCommitAuthority::GoalGenerationContinue {
                actor,
                finished_goal,
                verification_goal,
                decision_goal,
                predecessor,
            } => goal_generation_continue_authorized(
                &self.state,
                &self.issued_permits,
                actor,
                finished_goal,
                verification_goal,
                decision_goal,
                predecessor,
                batch,
                self.owner_epoch,
            ),
            BatchCommitAuthority::HistoricalToolResult { authority } => {
                historical_tool_result_commit_authorized(
                    &self.state,
                    self.cold_takeover_authority.as_ref(),
                    authority,
                    batch,
                    self.owner_epoch,
                )
            }
        };
        if !authorized {
            return Err(SurfaceCommitError::StalePublisherPermit);
        }
        if matches!(
            preflight_batch(batch),
            SurfaceCommitBatchPreflightResult::Rejected { .. }
        ) {
            return Err(SurfaceCommitError::OversizedBatch);
        }
        let batch_owner_epoch = match &batch.commit_class {
            CommitClass::Recorded {
                thread_owner_epoch, ..
            } => Some(thread_owner_epoch),
            CommitClass::Ephemeral { .. } => None,
        };
        if batch_owner_epoch.is_some_and(|epoch| {
            epoch != &self.owner_epoch
                && !self.recovered_prepared_authorizes_owner_transition(batch, epoch)
        }) {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        match &self.incomplete {
            Some(incomplete) if incomplete != batch => {
                return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
            }
            Some(_) => {}
            None if batch.cursor_before.next_seq.get() != self.next_sequence => {
                return match self
                    .ledger
                    .probe_commit(commit_id(&batch.commit_class), &batch.batch_digest)
                {
                    CommitProbe::Present(receipt) => Ok(SurfaceCommitApplied { receipt }),
                    _ => Err(SurfaceCommitError::CursorRangeAlreadyConsumed),
                };
            }
            None => {}
        }

        let candidate = match reduce_batch(SurfaceReduceMode::Live, &self.state, batch) {
            SurfaceReduceResult::Applied { state } => state,
            SurfaceReduceResult::AlreadyApplied { .. } => {
                let commit_id = commit_id(&batch.commit_class);
                return match self.ledger.probe_commit(commit_id, &batch.batch_digest) {
                    CommitProbe::Present(receipt) => Ok(SurfaceCommitApplied { receipt }),
                    _ => Err(SurfaceCommitError::CursorRangeAlreadyConsumed),
                };
            }
            SurfaceReduceResult::Rejected { error } => {
                return Err(SurfaceCommitError::InvalidBatch(error));
            }
        };

        let receipt = match self.ledger.append_complete_batch(batch) {
            Ok(receipt) => {
                self.next_sequence = batch.cursor_after.next_seq.get();
                self.incomplete = Some(batch.clone());
                receipt
            }
            Err(SurfaceLedgerError::PartialAppend) => {
                self.next_sequence = batch.cursor_after.next_seq.get();
                self.incomplete = Some(batch.clone());
                return Err(SurfaceCommitError::Ledger(
                    SurfaceLedgerError::PartialAppend,
                ));
            }
            Err(error) => return Err(SurfaceCommitError::Ledger(error)),
        };
        self.ledger
            .checkpoint(&receipt)
            .map_err(SurfaceCommitError::Ledger)?;
        let materialized = if projection_context.is_some() {
            self.materialize_projection(candidate)
        } else {
            Ok(candidate)
        };
        let materialized = match materialized {
            Ok(state) => state,
            Err(_) => {
                let context = projection_context.expect("projection context exists");
                let token = RetryLocalProjectionToken::new(
                    context.request_id.clone(),
                    context.target.clone(),
                    commit_id(&batch.commit_class).clone(),
                    self.owner_epoch,
                    context.fact_family,
                    batch.events.as_slice()[0].event_id.clone(),
                )
                .as_token();
                self.pending_projection = Some((token.clone(), batch.clone()));
                return Err(SurfaceCommitError::ProjectionPending { token });
            }
        };
        self.state = materialized;
        self.incomplete = None;
        if self.recovered_prepared.as_ref() == Some(batch) {
            self.recovered_prepared = None;
        }
        if let Some(hub) = &self.surface_hub {
            hub.apply_committed(std::sync::Arc::new(self.state.snapshot().clone()), batch);
        } else {
            self.recovered_publications.push(batch);
        }
        if extends_cold_recovery && let Some(authority) = self.cold_takeover_authority.as_mut() {
            for operation_id in batch.events.as_slice().iter().filter_map(|event| {
                let super::SurfaceEvent::Operation(super::OperationPatch::Requested { operation }) =
                    &event.event
                else {
                    return None;
                };
                Some(operation.operation_id.clone())
            }) {
                if !authority.recoverable_operations.contains(&operation_id) {
                    authority.recoverable_operations.push(operation_id);
                }
            }
        }
        Ok(SurfaceCommitApplied { receipt })
    }

    fn recovered_prepared_authorizes_owner_transition(
        &self,
        batch: &SurfaceCommitBatch,
        historical_epoch: &ThreadOwnerEpoch,
    ) -> bool {
        self.recovered_prepared.as_ref() == Some(batch)
            && self.state.snapshot().thread.owner_epoch == *historical_epoch
            && historical_epoch.get() < self.owner_epoch.get()
    }

    pub fn retry_projection(
        &mut self,
        token: &RetryProjectionToken,
    ) -> Result<SurfaceCommitApplied, SurfaceCommitError> {
        if !self
            .owner_lease
            .lease()
            .authorizes_thread(&self.state.snapshot().thread.thread_id)
        {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        let Some((expected, batch)) = self.pending_projection.clone() else {
            return Err(SurfaceCommitError::CursorRangeAlreadyConsumed);
        };
        if &expected != token {
            return Err(SurfaceCommitError::StaleOwnerEpoch);
        }
        let state = self.materialize_projection_batch(&batch).map_err(|_| {
            SurfaceCommitError::ProjectionPending {
                token: token.clone(),
            }
        })?;
        let receipt = match self
            .ledger
            .probe_commit(commit_id(&batch.commit_class), &batch.batch_digest)
        {
            CommitProbe::Present(receipt) => receipt,
            _ => {
                return Err(SurfaceCommitError::Ledger(
                    SurfaceLedgerError::CommitIdentityConflict,
                ));
            }
        };
        self.state = state;
        self.pending_projection = None;
        self.incomplete = None;
        if let Some(hub) = &self.surface_hub {
            hub.apply_committed(std::sync::Arc::new(self.state.snapshot().clone()), &batch);
        } else {
            self.recovered_publications.push(&batch);
        }
        Ok(SurfaceCommitApplied { receipt })
    }

    fn materialize_projection(
        &self,
        candidate: SurfaceReducerState,
    ) -> Result<SurfaceReducerState, ()> {
        #[cfg(test)]
        if self.projection_failure_injected {
            return Err(());
        }
        Ok(candidate)
    }

    fn materialize_projection_batch(
        &self,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceReducerState, ()> {
        let candidate = match reduce_batch(SurfaceReduceMode::Live, &self.state, batch) {
            SurfaceReduceResult::Applied { state } => state,
            _ => return Err(()),
        };
        self.materialize_projection(candidate)
    }

    #[cfg(test)]
    fn inject_projection_failure(&mut self, fail: bool) {
        self.projection_failure_injected = fail;
    }
}

fn task_reconciliation_payload(
    batch: &SurfaceCommitBatch,
) -> Option<(super::TaskRevision, &[super::SurfaceTask])> {
    let [event] = batch.events.as_slice() else {
        return None;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::Reconciled {
            source_revision,
            tasks,
        }),
    ) = (&event.scope, &event.event)
    else {
        return None;
    };
    Some((*source_revision, tasks))
}

fn actor_identity_authorizes_thread_batch(
    issued_permits: &[SurfacePublisherPermit],
    actor: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    matches!(
        actor,
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: permit_epoch,
            ..
        } if issued_permits.contains(actor)
            && *permit_epoch == owner_epoch
            && thread_id == &batch.cursor_before.thread_id
            && thread_id == &batch.cursor_after.thread_id
    )
}

fn historical_terminal_task_is_non_actionable(task: &super::SurfaceTask) -> bool {
    task.revision.get() == 1
        && task.task_type == super::SurfaceTaskType::MainSession
        && matches!(
            task.status,
            super::SurfaceTaskStatus::Completed
                | super::SurfaceTaskStatus::Stopped
                | super::SurfaceTaskStatus::Cancelled
        )
        && task.completed_at.is_some()
        && !task.backgrounded
        && task.parent_operation.is_none()
        && task.background_fence.is_none()
        && task.workflow_run_id.is_none()
        && task.subagent_id.is_none()
        && task.pending_interaction_id.is_none()
}

pub(crate) fn legacy_active_task_adoption_capability_fingerprint() -> super::Sha256Digest {
    super::Sha256Digest::digest(b"orca.runtime.legacy-active-task-adoption.v1")
}

fn active_task_adoption_record_matches_task(
    record: &LegacyActiveTaskAdoptionRecord,
    task: &super::SurfaceTask,
) -> bool {
    let expected_usage = record.usage().map(|usage| super::UsageTotals {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_tokens: usage.cache_tokens,
        estimated_cost_usd_micros: crate::cost::usd_to_micros(usage.estimated_cost_usd),
    });
    task.task_id.as_str() == record.id()
        && task.description == super::DisplayText::new(record.description())
        && task.created_at == super::UnixMillis::new(record.created_at_ms())
        && task.started_at == record.started_at_ms().map(super::UnixMillis::new)
        && task.usage == expected_usage
        && task.retry_count == record.retry_count()
        && task.output_truncated == record.output_truncated()
}

fn active_task_adoption_shape(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> Option<Vec<super::SurfaceTask>> {
    let snapshot = state.snapshot();
    if !actor_identity_authorizes_thread_batch(issued_permits, actor, batch, owner_epoch)
        || !matches!(
            batch.commit_class,
            CommitClass::Recorded {
                thread_owner_epoch,
                ..
            } if thread_owner_epoch == owner_epoch
        )
        || !matches!(
            snapshot.thread.persistence,
            super::ThreadPersistence::RecordedCatalogued
        )
        || snapshot.foreground_operation.is_some()
        || !snapshot.queued_operations.is_empty()
        || !snapshot.operation_history.is_empty()
        || !snapshot.background_operations.is_empty()
    {
        return None;
    }

    let events = batch.events.as_slice();
    if events.is_empty() || events.len() % 5 != 0 || batch.event_count as usize != events.len() {
        return None;
    }

    let existing_operations = snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .collect::<Vec<_>>();
    let mut operation_ids = existing_operations
        .iter()
        .map(|operation| operation.operation_id.clone())
        .collect::<BTreeSet<_>>();
    let mut request_ids = existing_operations
        .iter()
        .map(|operation| operation.request_id.clone())
        .collect::<BTreeSet<_>>();
    let mut last_reservation_sequence = existing_operations
        .iter()
        .map(|operation| operation.reservation.reservation_sequence.get())
        .max()
        .unwrap_or(0);
    let mut task_ids = snapshot
        .tasks
        .iter()
        .map(|task| task.task_id.clone())
        .collect::<BTreeSet<_>>();
    let mut background_fences = snapshot
        .background_operations
        .iter()
        .map(|operation| operation.fence.clone())
        .collect::<Vec<_>>();
    let mut tasks = Vec::with_capacity(events.len() / 5);
    let mut previous_task_id: Option<super::SurfaceTaskId> = None;
    let canonical_fingerprint = legacy_active_task_adoption_capability_fingerprint();
    let canonical_replayability = super::Replayability::NonReplayable {
        reason: super::NonReplayableReason::Missing,
        live_capsule: super::LiveOperationCapsule::Unavailable,
    };
    let expected_replayability_digest =
        super::canonical_replayability_digest(&canonical_replayability);
    let started_commit_id = commit_id(&batch.commit_class);

    for group in events.chunks_exact(5) {
        let [
            requested_event,
            admitted_event,
            started_event,
            task_event,
            transferred_event,
        ] = group
        else {
            return None;
        };
        let (
            SurfaceScope::Operation {
                operation_id: requested_scope_id,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::Requested { operation }),
        ) = (&requested_event.scope, &requested_event.event)
        else {
            return None;
        };
        let (
            SurfaceScope::Operation {
                operation_id: admitted_scope_id,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::Admitted {
                operation_id: admitted_operation_id,
                logical_turn_id,
                input,
                first_generation,
            }),
        ) = (&admitted_event.scope, &admitted_event.event)
        else {
            return None;
        };
        let (
            SurfaceScope::Generation {
                fence: started_scope_fence,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStarted {
                fence: started_fence,
                witness,
            }),
        ) = (&started_event.scope, &started_event.event)
        else {
            return None;
        };
        let (
            SurfaceScope::Thread,
            super::SurfaceEvent::Task(super::TaskPatch::Upserted {
                expected_revision,
                task,
            }),
        ) = (&task_event.scope, &task_event.event)
        else {
            return None;
        };
        let (
            SurfaceScope::Generation {
                fence: transferred_scope_fence,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationTransferred {
                fence: transferred_fence,
                background_fence,
                task_id: transferred_task_id,
            }),
        ) = (&transferred_event.scope, &transferred_event.event)
        else {
            return None;
        };

        let next_reservation_sequence = last_reservation_sequence.checked_add(1)?;
        let settings_match = matches!(
            &operation.intent.settings_receipt,
            super::OperationSettingsPreparationReceipt::Current {
                settings_revision,
                policy_epoch,
            } if *settings_revision == snapshot.settings.thread_revision
                && *policy_epoch == snapshot.settings.effective.policy_epoch
        );
        let canonical_operation = requested_scope_id == &operation.operation_id
            && admitted_scope_id == &operation.operation_id
            && admitted_operation_id == &operation.operation_id
            && operation.reservation.operation_id == operation.operation_id
            && operation.reservation.reservation_sequence.get() == next_reservation_sequence
            && matches!(operation.phase, super::OperationPhase::Requested)
            && !operation.ready_for_admission
            && operation.initial_logical_turn_id.is_none()
            && operation.initial_input_item_id.is_none()
            && operation.generations.is_empty()
            && operation.agent_loop_turns.is_empty()
            && operation.pending_control.is_none()
            && operation.finalization.is_none()
            && operation.terminal.is_none()
            && matches!(operation.intent.origin, super::OperationOrigin::TuiUser)
            && matches!(operation.intent.kind, super::OperationKind::UserTurn)
            && operation.intent.initial_replayability == canonical_replayability
            && matches!(
                operation.intent.busy_disposition,
                super::BusyDisposition::Queue
            )
            && matches!(
                operation.intent.interrupt_settlement,
                super::InterruptSettlement::SuspendUntilExplicitControl
            )
            && matches!(
                operation.intent.legacy_visibility,
                super::LegacyVisibility::PublishAfterAdmitted
            )
            && operation.intent.settings_revision == snapshot.settings.thread_revision
            && operation.intent.policy_epoch == snapshot.settings.effective.policy_epoch
            && operation.intent.required_capabilities.is_empty()
            && operation.intent.capability_fingerprint == canonical_fingerprint
            && settings_match;
        let canonical_generation = matches!(input, super::AdmittedInput::NotApplicable)
            && first_generation.fence.thread_id == snapshot.thread.thread_id
            && first_generation.fence.thread_owner_epoch == owner_epoch
            && first_generation.fence.operation_id == operation.operation_id
            && first_generation.fence.generation_id.get() == 0
            && first_generation.logical_turn_id == *logical_turn_id
            && matches!(
                first_generation.input,
                super::GenerationInputState::NotApplicable
            )
            && first_generation.predecessor.is_none()
            && matches!(first_generation.attempt, super::GenerationAttempt::Initial)
            && first_generation.goal_identity.is_none()
            && first_generation.replayability == canonical_replayability
            && first_generation.required_capabilities.is_empty()
            && first_generation.capability_fingerprint == canonical_fingerprint
            && matches!(first_generation.phase, super::GenerationPhase::Reserved)
            && first_generation.started_witness.is_none()
            && first_generation.stop_reason.is_none();
        let canonical_start = started_scope_fence == &first_generation.fence
            && started_fence == &first_generation.fence
            && witness.started_commit_id == *started_commit_id
            && witness.settings_revision == snapshot.settings.thread_revision
            && witness.policy_epoch == snapshot.settings.effective.policy_epoch
            && witness.durable_replayability_digest == expected_replayability_digest
            && witness.capability_fingerprint == canonical_fingerprint;
        let canonical_task = expected_revision.is_none()
            && task.revision.get() == 1
            && matches!(task.task_type, super::SurfaceTaskType::MainSession)
            && matches!(task.status, super::SurfaceTaskStatus::Running)
            && task.backgrounded
            && task.started_at.is_some()
            && task.completed_at.is_none()
            && task.parent_operation.as_ref() == Some(&operation.operation_id)
            && task.background_fence.as_ref() == Some(background_fence)
            && task.workflow_run_id.is_none()
            && task.subagent_id.is_none()
            && task.pending_interaction_id.is_none()
            && task.result.is_none()
            && task.error.is_none();
        let canonical_transfer = transferred_scope_fence == &first_generation.fence
            && transferred_fence == &first_generation.fence
            && background_fence.operation_fence == first_generation.fence
            && transferred_task_id.as_ref() == Some(&task.task_id);
        let identities_are_fresh = operation_ids.insert(operation.operation_id.clone())
            && request_ids.insert(operation.request_id.clone())
            && task_ids.insert(task.task_id.clone())
            && !background_fences.contains(background_fence)
            && previous_task_id
                .as_ref()
                .is_none_or(|previous| previous < &task.task_id);
        if !canonical_operation
            || !canonical_generation
            || !canonical_start
            || !canonical_task
            || !canonical_transfer
            || !identities_are_fresh
        {
            return None;
        }

        last_reservation_sequence = next_reservation_sequence;
        previous_task_id = Some(task.task_id.clone());
        background_fences.push(background_fence.clone());
        tasks.push(task.clone());
    }
    Some(tasks)
}

fn recovered_active_task_adoption_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    active_task_adoption_shape(state, issued_permits, actor, batch, owner_epoch).is_some()
}

fn active_task_adoption_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor: &SurfacePublisherPermit,
    receipt: &LegacyActiveTaskAdoptionReceipt,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !receipt.is_valid()
        || receipt.session_id()
            != uuid::Uuid::from_bytes(*state.snapshot().thread.thread_id.as_bytes()).to_string()
        || receipt
            .records()
            .windows(2)
            .any(|records| records[0].id() >= records[1].id())
        || receipt.records().iter().any(|record| {
            record.publication_revision() == 0
                || record.publication_revision() >= receipt.publication_horizon()
        })
    {
        return false;
    }
    let Some(tasks) = active_task_adoption_shape(state, issued_permits, actor, batch, owner_epoch)
    else {
        return false;
    };
    let missing_records = receipt
        .records()
        .iter()
        .filter(|record| {
            !state
                .snapshot()
                .tasks
                .iter()
                .any(|task| task.task_id.as_str() == record.id())
        })
        .collect::<Vec<_>>();
    !missing_records.is_empty()
        && tasks.len() == missing_records.len()
        && missing_records
            .into_iter()
            .zip(tasks.iter())
            .all(|(record, task)| active_task_adoption_record_matches_task(record, task))
}

fn recovered_terminal_task_reconciliation_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !actor_identity_authorizes_thread_batch(issued_permits, actor, batch, owner_epoch)
        || !matches!(batch.commit_class, CommitClass::Recorded { .. })
        || !matches!(
            state.snapshot().thread.persistence,
            super::ThreadPersistence::RecordedCatalogued
        )
    {
        return false;
    }
    let Some((source_revision, tasks)) = task_reconciliation_payload(batch) else {
        return false;
    };
    let unique_ids = tasks
        .iter()
        .map(|task| &task.task_id)
        .collect::<BTreeSet<_>>()
        .len()
        == tasks.len();
    if !unique_ids
        || tasks.iter().any(|task| task.revision > source_revision)
        || state.snapshot().tasks.iter().any(|current| {
            tasks.iter().find(|task| task.task_id == current.task_id) != Some(current)
        })
    {
        return false;
    }
    let additions = tasks
        .iter()
        .filter(|task| {
            !state
                .snapshot()
                .tasks
                .iter()
                .any(|current| current.task_id == task.task_id)
        })
        .collect::<Vec<_>>();
    !additions.is_empty()
        && additions
            .into_iter()
            .all(historical_terminal_task_is_non_actionable)
}

fn terminal_task_reconciliation_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor: &SurfacePublisherPermit,
    receipt: &LegacyTerminalTaskReconciliationReceipt,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !receipt.is_valid()
        || receipt.session_id()
            != uuid::Uuid::from_bytes(*state.snapshot().thread.thread_id.as_bytes()).to_string()
        || !recovered_terminal_task_reconciliation_authorized(
            state,
            issued_permits,
            actor,
            batch,
            owner_epoch,
        )
    {
        return false;
    }
    let Some((source_revision, tasks)) = task_reconciliation_payload(batch) else {
        return false;
    };
    if source_revision.get() < receipt.publication_horizon() {
        return false;
    }
    let mut expected = state.snapshot().tasks.clone();
    expected.extend(
        receipt
            .reconciled_surface_tasks()
            .into_iter()
            .filter(|candidate| {
                !state
                    .snapshot()
                    .tasks
                    .iter()
                    .any(|current| current.task_id == candidate.task_id)
            }),
    );
    tasks == expected
}

fn historical_tool_result_commit_authorized(
    state: &SurfaceReducerState,
    cold_owner: Option<&ColdOwnerTakeoverAuthority>,
    authority: &HistoricalToolResultCommitAuthority,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    let snapshot = state.snapshot();
    let Some(cold_owner) = cold_owner else {
        return false;
    };
    if authority.current_owner_epoch != owner_epoch
        || authority.current_incarnation != snapshot.cursor.incarnation
        || authority.historical_fence.thread_id != snapshot.thread.thread_id
        || authority.invocation_started.invocation_id() != &authority.invocation_id
        || authority.invocation_started.fence() != &authority.historical_fence
        || authority.invocation_started.revision() != authority.expected_projection_revision
        || !cold_owner.authorizes_historical_commit(
            &authority.historical_fence,
            snapshot,
            &authority.current_incarnation,
            owner_epoch,
        )
        || !matches!(
            &batch.commit_class,
            CommitClass::Recorded {
                thread_owner_epoch,
                ..
            } if *thread_owner_epoch == owner_epoch
        )
    {
        return false;
    }
    let Some(generation) = snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .find(|operation| operation.operation_id == authority.historical_fence.operation_id)
        .and_then(|operation| {
            operation
                .generations
                .iter()
                .find(|generation| generation.fence == authority.historical_fence)
        })
    else {
        return false;
    };
    let Some(tool) = snapshot
        .tools
        .iter()
        .find(|tool| tool.request.tool_call_id == authority.invocation_id)
    else {
        return false;
    };
    if tool.request.turn_id != generation.logical_turn_id
        || tool.state != super::SurfaceToolViewState::Running
        || tool.result.is_some()
        || tool.invocation_started.as_ref() != Some(&authority.invocation_started)
    {
        return false;
    }
    let [completed, item] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Generation {
            fence: completed_fence,
        },
        super::SurfaceEvent::Tool(super::ToolPatch::Completed { result }),
    ) = (&completed.scope, &completed.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Generation { fence: item_fence },
        super::SurfaceEvent::Item(super::ItemPatch::Added {
            item:
                super::SurfaceItem::ToolResultMessage {
                    turn_id,
                    tool_call_id,
                    content,
                    terminal,
                    pinned,
                    ..
                },
        }),
    ) = (&item.scope, &item.event)
    else {
        return false;
    };
    completed_fence == &authority.historical_fence
        && item_fence == &authority.historical_fence
        && result.tool_call_id == authority.invocation_id
        && result.name == tool.request.name
        && matches!(
            &result.terminal,
            super::SurfaceToolTerminal {
                source: super::ToolTerminalSource::Observed,
                invocation_started: super::ToolInvocationStarted::Yes,
                ..
            }
        )
        && turn_id == &tool.request.turn_id
        && tool_call_id == &authority.invocation_id
        && terminal == &result.terminal
        && result.output.as_ref().or(result.error.as_ref()) == Some(content)
        && !pinned
}

fn permit_authorizes(
    issued_permits: &[SurfacePublisherPermit],
    permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(permit) {
        return false;
    }
    match permit {
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: permit_epoch,
            ..
        } => {
            thread_id == &batch.cursor_before.thread_id
                && *permit_epoch == owner_epoch
                && batch.cursor_after.thread_id == *thread_id
                && (batch.events.as_slice().iter().all(|event| {
                    !matches!(
                        &event.scope,
                        SurfaceScope::Generation { .. }
                            | SurfaceScope::Background { .. }
                            | SurfaceScope::Goal { .. }
                    ) && !matches!(&event.event, super::SurfaceEvent::Goal(_))
                        && !matches!(
                            &event.event,
                            super::SurfaceEvent::Operation(
                                super::OperationPatch::FinalizationStarted { .. }
                                    | super::OperationPatch::FinalizationSettlementRecorded { .. }
                                    | super::OperationPatch::FinalizationDegraded { .. }
                                    | super::OperationPatch::Terminal { .. }
                            )
                        )
                        && !matches!(
                            &event.event,
                            super::SurfaceEvent::Task(super::TaskPatch::Reconciled { .. })
                        )
                }) || actor_control_workflow_launch_authorized(batch)
                    || actor_control_main_session_transfer_authorized(batch)
                    || actor_control_admission_pair_authorized(batch)
                    || actor_control_resume_pair_authorized(batch))
        }
        SurfacePublisherPermit::Generation { fence, .. } => batch
            .events
            .as_slice()
            .iter()
            .all(|event| matches!(&event.scope, SurfaceScope::Generation { fence: scope } if scope == fence)),
        SurfacePublisherPermit::Background { fence, .. } => batch
            .events
            .as_slice()
            .iter()
            .all(|event| matches!(&event.scope, SurfaceScope::Background { fence: scope } if scope == fence)),
        SurfacePublisherPermit::Goal {
            goal_fence,
            receipt_digest,
            ..
        } => batch.events.as_slice().iter().all(|event| {
            matches!(
                (&event.scope, &event.event),
                (
                    SurfaceScope::Goal { goal_id, .. },
                    super::SurfaceEvent::Goal(envelope),
                ) if goal_id == &goal_fence.goal_id
                    && envelope.receipt.goal_id == goal_fence.goal_id
                    && envelope.receipt.goal_revision == goal_fence.goal_revision
                    && envelope.receipt.goal_owner_epoch == goal_fence.goal_owner_epoch
                    && envelope.receipt.receipt_digest == *receipt_digest
            )
        }),
        SurfacePublisherPermit::Finalizer {
            operation_id,
            finalize_intent_id,
            owner_epoch: permit_epoch,
            ..
        } => {
            *permit_epoch == owner_epoch
                && batch.events.as_slice().iter().all(|event| {
                    finalizer_event_authorized(operation_id, finalize_intent_id, event)
                })
        }
        SurfacePublisherPermit::Recovery {
            current_owner_epoch,
            historical_fence,
            ..
        } => {
            *current_owner_epoch == owner_epoch
                && historical_fence.thread_id == batch.cursor_before.thread_id
                && batch.cursor_after.thread_id == historical_fence.thread_id
                && recovery_batch_authorized(historical_fence, batch)
        }
    }
}

fn actor_goal_run_start_authorized(
    issued_permits: &[SurfacePublisherPermit],
    actor: &SurfacePublisherPermit,
    goal: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor) || !issued_permits.contains(goal) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Goal {
            goal_fence,
            receipt_digest,
            ..
        },
    ) = (actor, goal)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let events = batch.events.as_slice();
    if let [operation_event, item_event, goal_event] = events {
        let (
            SurfaceScope::Goal {
                goal_id: scoped_goal_id,
                causative_generation: Some(causative_generation),
            },
            super::SurfaceEvent::Goal(goal_envelope),
        ) = (&goal_event.scope, &goal_event.event)
        else {
            return false;
        };
        let super::GoalPatch::OuterTurnStarted { identity } = &goal_envelope.patch else {
            return false;
        };
        if scoped_goal_id != &goal_fence.goal_id
            || goal_envelope.receipt.goal_id != goal_fence.goal_id
            || goal_envelope.receipt.goal_revision != goal_fence.goal_revision
            || goal_envelope.receipt.goal_owner_epoch != goal_fence.goal_owner_epoch
            || goal_envelope.receipt.receipt_digest != *receipt_digest
            || causative_generation != &identity.operation_fence
            || identity.goal_id != goal_fence.goal_id
        {
            return false;
        }
        let (
            SurfaceScope::Operation {
                operation_id: scoped_operation_id,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::Admitted {
                operation_id,
                logical_turn_id,
                input,
                first_generation,
            }),
        ) = (&operation_event.scope, &operation_event.event)
        else {
            return false;
        };
        let (
            super::AdmittedInput::PendingUser {
                item_id: admitted_item_id,
                presentation: admitted_presentation,
                correlation_id: admitted_correlation,
            },
            super::GenerationInputState::Pending {
                input_item_id,
                presentation,
                correlation_id,
            },
        ) = (input, &first_generation.input)
        else {
            return false;
        };
        let (
            SurfaceScope::Generation { fence: item_fence },
            super::SurfaceEvent::Item(super::ItemPatch::Added {
                item:
                    super::SurfaceItem::UserMessage {
                        id: item_id,
                        turn_id,
                        input:
                            super::SurfaceUserInputState::Pending {
                                presentation: item_presentation,
                                correlation_id: item_correlation,
                            },
                        ..
                    },
            }),
        ) = (&item_event.scope, &item_event.event)
        else {
            return false;
        };
        return scoped_operation_id == operation_id
            && operation_id == &identity.operation_fence.operation_id
            && logical_turn_id == &identity.logical_turn_id
            && first_generation.fence == identity.operation_fence
            && first_generation.logical_turn_id == identity.logical_turn_id
            && first_generation.goal_identity.as_ref() == Some(identity)
            && item_fence == &identity.operation_fence
            && admitted_item_id == &identity.canonical_input_item_id
            && input_item_id == &identity.canonical_input_item_id
            && item_id == &identity.canonical_input_item_id
            && turn_id == &identity.logical_turn_id
            && admitted_presentation == presentation
            && presentation == item_presentation
            && admitted_correlation == correlation_id
            && correlation_id == item_correlation;
    }
    let [goal_event, operation_event] = events else {
        return false;
    };
    let (
        SurfaceScope::Goal {
            goal_id: scoped_goal_id,
            causative_generation: None,
        },
        super::SurfaceEvent::Goal(goal_envelope),
    ) = (&goal_event.scope, &goal_event.event)
    else {
        return false;
    };
    if scoped_goal_id != &goal_fence.goal_id
        || goal_envelope.receipt.goal_id != goal_fence.goal_id
        || goal_envelope.receipt.goal_revision != goal_fence.goal_revision
        || goal_envelope.receipt.goal_owner_epoch != goal_fence.goal_owner_epoch
        || goal_envelope.receipt.receipt_digest != *receipt_digest
    {
        return false;
    }
    if let super::GoalPatch::Paused {
        goal_id,
        goal_run_id: Some(_),
        state:
            super::SurfaceGoalState::Paused {
                reason: super::SurfaceGoalPauseReason::User,
                ..
            },
        ..
    } = &goal_envelope.patch
    {
        let (
            SurfaceScope::Operation {
                operation_id: scoped_operation_id,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
                operation_id,
                request_id: _,
                intent:
                    super::PendingControlIntent::Terminalize {
                        operation_id: intent_operation_id,
                        cause: super::TerminalizationCause::GoalPause,
                    },
            }),
        ) = (&operation_event.scope, &operation_event.event)
        else {
            return false;
        };
        return goal_id == &goal_fence.goal_id
            && scoped_operation_id == operation_id
            && operation_id == intent_operation_id;
    }
    let (run, objective_revision) = match &goal_envelope.patch {
        super::GoalPatch::Created { goal }
            if goal.goal_id == goal_fence.goal_id
                && goal.goal_revision == goal_fence.goal_revision
                && goal.current_run.is_some() =>
        {
            (
                goal.current_run.as_ref().expect("guarded current run"),
                goal.objective_revision,
            )
        }
        super::GoalPatch::RunStarted { goal_id, goal_run } if goal_id == &goal_fence.goal_id => {
            (goal_run, goal_envelope.receipt.objective_revision)
        }
        _ => return false,
    };
    let (
        SurfaceScope::Operation {
            operation_id: scoped_operation_id,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::Requested { operation }),
    ) = (&operation_event.scope, &operation_event.event)
    else {
        return false;
    };
    scoped_operation_id == &run.operation_id
        && operation.operation_id == run.operation_id
        && operation.reservation.operation_id == run.operation_id
        && operation.phase == super::OperationPhase::Requested
        && matches!(
            &operation.intent.kind,
            super::OperationKind::GoalRun {
                goal_id,
                goal_run_id,
                initial_objective_revision,
            } if goal_id == &goal_fence.goal_id
                && goal_run_id == &run.goal_run_id
                && initial_objective_revision == &objective_revision
        )
}

fn actor_goal_edit_run_authorized(
    issued_permits: &[SurfacePublisherPermit],
    actor: &SurfacePublisherPermit,
    first_goal: &SurfacePublisherPermit,
    second_goal: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor)
        || !issued_permits.contains(first_goal)
        || !issued_permits.contains(second_goal)
    {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Goal {
            goal_fence: first_fence,
            receipt_digest: first_digest,
            ..
        },
        SurfacePublisherPermit::Goal {
            goal_fence: second_fence,
            receipt_digest: second_digest,
            ..
        },
    ) = (actor, first_goal, second_goal)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
        || first_fence.goal_id != second_fence.goal_id
        || first_fence.goal_owner_epoch != second_fence.goal_owner_epoch
        || first_fence.goal_revision.get().checked_add(1) != Some(second_fence.goal_revision.get())
    {
        return false;
    }
    let [first_event, second_event, operation_event] = batch.events.as_slice() else {
        return false;
    };
    let (super::SurfaceEvent::Goal(first_envelope), super::SurfaceEvent::Goal(second_envelope)) =
        (&first_event.event, &second_event.event)
    else {
        return false;
    };
    let first_matches = first_envelope.receipt.goal_id == first_fence.goal_id
        && first_envelope.receipt.goal_revision == first_fence.goal_revision
        && first_envelope.receipt.goal_owner_epoch == first_fence.goal_owner_epoch
        && first_envelope.receipt.receipt_digest == *first_digest
        && matches!(
            &first_envelope.patch,
            super::GoalPatch::Edited {
                goal_id,
                goal,
                ..
            } if goal_id == &first_fence.goal_id
                && goal.goal_id == first_fence.goal_id
                && goal.current_run.is_none()
        );
    let run = match &second_envelope.patch {
        super::GoalPatch::RunStarted { goal_id, goal_run } if goal_id == &second_fence.goal_id => {
            goal_run
        }
        _ => return false,
    };
    let second_matches = second_envelope.receipt.goal_id == second_fence.goal_id
        && second_envelope.receipt.goal_revision == second_fence.goal_revision
        && second_envelope.receipt.goal_owner_epoch == second_fence.goal_owner_epoch
        && second_envelope.receipt.receipt_digest == *second_digest;
    let (
        SurfaceScope::Operation {
            operation_id: scoped_operation_id,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::Requested { operation }),
    ) = (&operation_event.scope, &operation_event.event)
    else {
        return false;
    };
    first_matches
        && second_matches
        && scoped_operation_id == &run.operation_id
        && operation.operation_id == run.operation_id
        && operation.phase == super::OperationPhase::Requested
        && matches!(
            &operation.intent.kind,
            super::OperationKind::GoalRun {
                goal_id,
                goal_run_id,
                initial_objective_revision,
            } if goal_id == &second_fence.goal_id
                && goal_run_id == &run.goal_run_id
                && initial_objective_revision == &second_envelope.receipt.objective_revision
        )
}

fn actor_generation_terminalization_authorized(
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    generation_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(generation_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_owner_epoch,
            ..
        },
        SurfacePublisherPermit::Generation { fence, .. },
    ) = (actor_permit, generation_permit)
    else {
        return false;
    };
    if *actor_owner_epoch != owner_epoch
        || thread_id != &fence.thread_id
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let Some((intent, settlements)) = batch.events.as_slice().split_first() else {
        return false;
    };
    let cause = match (&intent.scope, &intent.event) {
        (
            SurfaceScope::Operation {
                operation_id: scoped_operation,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
                operation_id,
                intent:
                    super::PendingControlIntent::Terminalize {
                        operation_id: intent_operation,
                        cause,
                    },
                ..
            }),
        ) if scoped_operation == &fence.operation_id
            && operation_id == &fence.operation_id
            && intent_operation == &fence.operation_id =>
        {
            *cause
        }
        _ => return false,
    };
    let mut index = 0usize;
    let mut required_terminal_tool_ids = BTreeSet::new();
    let mut terminalized_tool_ids = BTreeSet::new();
    while index < settlements.len() {
        let event = &settlements[index];
        let authorized = match (&event.scope, &event.event) {
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Interaction(super::InteractionPatch::Cancelled {
                    reason, ..
                }),
            ) if scope == fence => matches!(
                (cause, reason),
                (
                    super::TerminalizationCause::HostShutdown,
                    super::InteractionCancelReason::HostShutdown,
                ) | (
                    super::TerminalizationCause::ThreadClose,
                    super::InteractionCancelReason::ThreadClose,
                ) | (
                    super::TerminalizationCause::UserCancel,
                    super::InteractionCancelReason::OperationCancelled {
                        reason: super::CancelReason::User,
                    },
                ) | (
                    super::TerminalizationCause::GoalPause,
                    super::InteractionCancelReason::OperationCancelled {
                        reason: super::CancelReason::GoalPause,
                    },
                )
            ),
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                    call:
                        super::SurfaceCapabilityCall {
                            fence: call_fence,
                            kind:
                                super::SurfaceCapabilityCallKind::ReadTextFile
                                | super::SurfaceCapabilityCallKind::TerminalOutput
                                | super::SurfaceCapabilityCallKind::TerminalWaitForExit,
                            state:
                                super::SurfaceCapabilityCallState::FailedBeforeWrite { .. }
                                | super::SurfaceCapabilityCallState::ObservationUnavailable { .. },
                            ..
                        },
                }),
            ) => scope == fence && call_fence == fence,
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                    call:
                        super::SurfaceCapabilityCall {
                            fence: call_fence,
                            kind:
                                super::SurfaceCapabilityCallKind::TerminalKill
                                | super::SurfaceCapabilityCallKind::TerminalRelease,
                            state: super::SurfaceCapabilityCallState::DeliveryPossible,
                            ..
                        },
                }),
            ) if scope == fence && call_fence == fence => {
                let Some((consumed, owning_tool_call_id)) =
                    recovery_terminal_cleanup_sequence_authorized(fence, settlements, index)
                else {
                    return false;
                };
                if !settlements[index..index + consumed].iter().all(
                    |event| matches!(&event.scope, SurfaceScope::Generation { fence: scope } if scope == fence),
                ) {
                    return false;
                }
                required_terminal_tool_ids.insert(owning_tool_call_id);
                index += consumed.saturating_sub(1);
                true
            }
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                    call:
                        super::SurfaceCapabilityCall {
                            fence: call_fence,
                            kind: super::SurfaceCapabilityCallKind::TerminalKill,
                            state: super::SurfaceCapabilityCallState::Prepared,
                            ..
                        },
                }),
            ) if scope == fence && call_fence == fence => {
                let Some(additional_events) = live_terminal_cleanup_terminalization_authorized(
                    fence,
                    settlements,
                    index,
                    &mut required_terminal_tool_ids,
                ) else {
                    return false;
                };
                index += additional_events;
                true
            }
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                    call:
                        super::SurfaceCapabilityCall {
                            fence: call_fence,
                            kind:
                                super::SurfaceCapabilityCallKind::WriteTextFile
                                | super::SurfaceCapabilityCallKind::TerminalCreate,
                            state: super::SurfaceCapabilityCallState::FailedBeforeWrite { .. },
                            ..
                        },
                }),
            ) => scope == fence && call_fence == fence,
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                    call:
                        super::SurfaceCapabilityCall {
                            fence: call_fence,
                            kind: super::SurfaceCapabilityCallKind::WriteTextFile,
                            owning_tool_call_id,
                            state:
                                super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                    effect_kind: super::ExternalEffectKind::FileWrite,
                                    ..
                                },
                            ..
                        },
                }),
            ) if scope == fence && call_fence == fence => {
                required_terminal_tool_ids.insert(owning_tool_call_id.clone());
                true
            }
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                    call:
                        super::SurfaceCapabilityCall {
                            call_id,
                            fence: call_fence,
                            kind: super::SurfaceCapabilityCallKind::TerminalCreate,
                            owning_tool_call_id,
                            state:
                                super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                    effect_kind: super::ExternalEffectKind::TerminalCreate,
                                    ..
                                },
                            ..
                        },
                }),
            ) if scope == fence && call_fence == fence => {
                let Some(lease_event) = settlements.get(index + 1) else {
                    return false;
                };
                let (
                    SurfaceScope::Generation { fence: lease_fence },
                    super::SurfaceEvent::Tool(super::ToolPatch::RemoteTerminalLeaseChanged {
                        lease:
                            super::SurfaceRemoteTerminalLease {
                                owning_tool_call_id: lease_tool_call_id,
                                state:
                                    super::SurfaceRemoteTerminalLeaseState::IdentityUnknown {
                                        create_call_id,
                                    },
                                ..
                            },
                    }),
                ) = (&lease_event.scope, &lease_event.event)
                else {
                    return false;
                };
                if lease_fence != fence
                    || create_call_id != call_id
                    || lease_tool_call_id != owning_tool_call_id
                {
                    return false;
                }
                required_terminal_tool_ids.insert(owning_tool_call_id.clone());
                index += 1;
                true
            }
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                    call:
                        super::SurfaceCapabilityCall {
                            fence: call_fence,
                            kind:
                                super::SurfaceCapabilityCallKind::TerminalKill
                                | super::SurfaceCapabilityCallKind::TerminalRelease,
                            owning_tool_call_id,
                            state:
                                super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                    effect_kind:
                                        super::ExternalEffectKind::TerminalKill
                                        | super::ExternalEffectKind::TerminalRelease,
                                    ..
                                },
                            ..
                        },
                }),
            ) if scope == fence && call_fence == fence => {
                let Some(lease_event) = settlements.get(index + 1) else {
                    return false;
                };
                let (
                    SurfaceScope::Generation { fence: lease_fence },
                    super::SurfaceEvent::Tool(super::ToolPatch::RemoteTerminalLeaseChanged {
                        lease:
                            super::SurfaceRemoteTerminalLease {
                                owning_tool_call_id: lease_tool_call_id,
                                state:
                                    super::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                                        terminal_id: Some(_),
                                        owner_fence,
                                    },
                                ..
                            },
                    }),
                ) = (&lease_event.scope, &lease_event.event)
                else {
                    return false;
                };
                if lease_fence != fence
                    || owner_fence != fence
                    || lease_tool_call_id != owning_tool_call_id
                {
                    return false;
                }
                required_terminal_tool_ids.insert(owning_tool_call_id.clone());
                index += 1;
                true
            }
            (
                SurfaceScope::Generation { fence: scope },
                super::SurfaceEvent::Tool(super::ToolPatch::Completed { result }),
            ) if scope == fence && required_terminal_tool_ids.contains(&result.tool_call_id) => {
                let Some(completion_events) = ambiguous_tool_completion_authorized(
                    fence,
                    settlements,
                    index,
                    &result.tool_call_id,
                    &mut terminalized_tool_ids,
                ) else {
                    return false;
                };
                index += completion_events.saturating_sub(1);
                true
            }
            _ => false,
        };
        if !authorized {
            return false;
        }
        index += 1;
    }
    required_terminal_tool_ids == terminalized_tool_ids
}

fn live_terminal_cleanup_terminalization_authorized(
    fence: &super::SurfaceOperationFence,
    settlements: &[super::SurfaceEventEnvelope],
    index: usize,
    required_terminal_tool_ids: &mut BTreeSet<super::SurfaceToolCallId>,
) -> Option<usize> {
    let Some(prepared_event) = settlements.get(index) else {
        return None;
    };
    let Some(delivery_event) = settlements.get(index + 1) else {
        return None;
    };
    let Some(ambiguous_event) = settlements.get(index + 2) else {
        return None;
    };
    let Some(lease_event) = settlements.get(index + 3) else {
        return None;
    };
    let (
        SurfaceScope::Generation {
            fence: prepared_fence,
        },
        super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged { call: prepared }),
    ) = (&prepared_event.scope, &prepared_event.event)
    else {
        return None;
    };
    let (
        SurfaceScope::Generation {
            fence: delivery_fence,
        },
        super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged { call: delivery }),
    ) = (&delivery_event.scope, &delivery_event.event)
    else {
        return None;
    };
    let (
        SurfaceScope::Generation {
            fence: ambiguous_fence,
        },
        super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged { call: ambiguous }),
    ) = (&ambiguous_event.scope, &ambiguous_event.event)
    else {
        return None;
    };
    let (
        SurfaceScope::Generation { fence: lease_fence },
        super::SurfaceEvent::Tool(super::ToolPatch::RemoteTerminalLeaseChanged { lease }),
    ) = (&lease_event.scope, &lease_event.event)
    else {
        return None;
    };
    let super::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
        terminal_id: Some(terminal_id),
        owner_fence,
    } = &lease.state
    else {
        return None;
    };
    if !(prepared_fence == fence
        && delivery_fence == fence
        && ambiguous_fence == fence
        && lease_fence == fence
        && prepared.fence == *fence
        && prepared.kind == super::SurfaceCapabilityCallKind::TerminalKill
        && prepared.state == super::SurfaceCapabilityCallState::Prepared
        && same_capability_call_identity(prepared, delivery)
        && delivery.state == super::SurfaceCapabilityCallState::DeliveryPossible
        && same_capability_call_identity(prepared, ambiguous)
        && matches!(
            ambiguous.state,
            super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                effect_kind: super::ExternalEffectKind::TerminalKill,
                ..
            }
        )
        && lease.owning_tool_call_id == prepared.owning_tool_call_id
        && owner_fence == fence
        && prepared.arguments_digest
            == super::Sha256Digest::new(
                sha2::Sha256::digest(terminal_id.as_str().as_bytes()).into(),
            ))
    {
        return None;
    }
    required_terminal_tool_ids.insert(prepared.owning_tool_call_id.clone());
    Some(3)
}

fn ambiguous_tool_completion_authorized(
    fence: &super::SurfaceOperationFence,
    settlements: &[super::SurfaceEventEnvelope],
    index: usize,
    owning_tool_call_id: &super::SurfaceToolCallId,
    terminalized_tool_ids: &mut BTreeSet<super::SurfaceToolCallId>,
) -> Option<usize> {
    if !terminalized_tool_ids.insert(owning_tool_call_id.clone()) {
        return Some(0);
    }
    let completed = settlements.get(index)?;
    let item = settlements.get(index + 1)?;
    let (
        SurfaceScope::Generation {
            fence: completed_fence,
        },
        super::SurfaceEvent::Tool(super::ToolPatch::Completed { result }),
    ) = (&completed.scope, &completed.event)
    else {
        return None;
    };
    let (
        SurfaceScope::Generation { fence: item_fence },
        super::SurfaceEvent::Item(super::ItemPatch::Added {
            item:
                super::SurfaceItem::ToolResultMessage {
                    tool_call_id,
                    terminal,
                    ..
                },
        }),
    ) = (&item.scope, &item.event)
    else {
        return None;
    };
    if completed_fence != fence
        || item_fence != fence
        || &result.tool_call_id != owning_tool_call_id
        || tool_call_id != owning_tool_call_id
        || result.terminal != *terminal
        || !matches!(
            result.terminal,
            super::SurfaceToolTerminal {
                kind: super::SurfaceToolResultKind::ExternalEffectAmbiguous,
                source: super::ToolTerminalSource::Observed,
                invocation_started: super::ToolInvocationStarted::Yes,
            }
        )
    {
        return None;
    }
    Some(2)
}

fn same_capability_call_identity(
    left: &super::SurfaceCapabilityCall,
    right: &super::SurfaceCapabilityCall,
) -> bool {
    left.call_id == right.call_id
        && left.acp_session_id == right.acp_session_id
        && left.fence == right.fence
        && left.capability_revision == right.capability_revision
        && left.policy_epoch == right.policy_epoch
        && left.kind == right.kind
        && left.arguments_digest == right.arguments_digest
        && left.owning_tool_call_id == right.owning_tool_call_id
}

fn live_generation_suspend_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    generation_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(generation_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_owner_epoch,
            ..
        },
        SurfacePublisherPermit::Generation { fence, .. },
    ) = (actor_permit, generation_permit)
    else {
        return false;
    };
    if *actor_owner_epoch != owner_epoch
        || thread_id != &fence.thread_id
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let events = batch.events.as_slice();
    let (suspended, prefix) = if events.len() >= 4
        && queued_interrupted_resume_suffix_authorized(state, fence, &events[events.len() - 2..])
    {
        (&events[events.len() - 3], &events[..events.len() - 3])
    } else {
        let Some((suspended, prefix)) = events.split_last() else {
            return false;
        };
        (suspended, prefix)
    };
    let Some((stopped, stream_discards)) = prefix.split_last() else {
        return false;
    };
    if !matches!(
        (&stopped.scope, &stopped.event),
        (
            SurfaceScope::Generation { fence: scope },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                reason: super::GenerationStopReason::InterruptedResumable,
                ..
            }),
        ) if scope == fence && patch_fence == fence
    ) || !matches!(
        (&suspended.scope, &suspended.event),
        (
            SurfaceScope::Operation {
                operation_id: scoped_operation,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::Suspended {
                operation_id,
                cause: super::SuspensionCause::Interrupted { generation_id },
            }),
        ) if scoped_operation == &fence.operation_id
            && operation_id == &fence.operation_id
            && generation_id == &fence.generation_id
    ) {
        return false;
    }
    stream_discards_cover_open_streams(
        state,
        fence,
        &SurfaceScope::Generation {
            fence: fence.clone(),
        },
        super::AssistantDiscardReason::GenerationInterrupted,
        stream_discards,
    )
}

fn queued_interrupted_resume_suffix_authorized(
    state: &SurfaceReducerState,
    interrupted: &super::SurfaceOperationFence,
    suffix: &[super::SurfaceEventEnvelope],
) -> bool {
    let [reserved, resume] = suffix else {
        return false;
    };
    let (
        SurfaceScope::Generation {
            fence: reserved_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationReserved { generation }),
    ) = (&reserved.scope, &reserved.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Operation {
            operation_id: resume_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
            operation_id,
            request_id,
            intent: super::PendingControlIntent::ResumeStarting { generation_fence },
        }),
    ) = (&resume.scope, &resume.event)
    else {
        return false;
    };
    let Some(operation) = state
        .snapshot()
        .foreground_operation
        .as_ref()
        .filter(|operation| operation.operation_id == interrupted.operation_id)
    else {
        return false;
    };
    reserved_scope == &generation.fence
        && generation_fence == &generation.fence
        && resume_scope == &interrupted.operation_id
        && operation_id == &interrupted.operation_id
        && request_id == &operation.request_id
        && generation.predecessor.as_ref() == Some(interrupted)
        && generation.fence.operation_id == interrupted.operation_id
        && generation.fence.generation_id.get() == interrupted.generation_id.get().saturating_add(1)
        && generation.phase == super::GenerationPhase::Reserved
        && matches!(
            &operation.pending_control,
            Some(super::PendingControlIntent::ResumeAfterInterruptedStop {
                generation_fence,
            }) if generation_fence == interrupted
        )
}

fn live_generation_stop_disposition_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    generation_permit: &SurfacePublisherPermit,
    finalizer_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(generation_permit) || !issued_permits.contains(finalizer_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::Generation { fence, .. },
        SurfacePublisherPermit::Finalizer {
            operation_id,
            finalize_intent_id,
            owner_epoch: finalizer_owner_epoch,
            ..
        },
    ) = (generation_permit, finalizer_permit)
    else {
        return false;
    };
    if *finalizer_owner_epoch != owner_epoch || operation_id != &fence.operation_id {
        return false;
    }
    let Some((finalization, prefix)) = batch.events.as_slice().split_last() else {
        return false;
    };
    let Some((stop, stream_discards)) = prefix.split_last() else {
        return false;
    };
    let stop_reason = match (&stop.scope, &stop.event) {
        (
            SurfaceScope::Generation { fence: scope },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                reason,
                ..
            }),
        ) if scope == fence && patch_fence == fence => reason,
        _ => return false,
    };
    let expected_discard_reason = match stop_reason {
        super::GenerationStopReason::Cancelled { .. } => {
            super::AssistantDiscardReason::GenerationCancelled
        }
        super::GenerationStopReason::InterruptedResumable => {
            super::AssistantDiscardReason::GenerationInterrupted
        }
        super::GenerationStopReason::RuntimeRestart
        | super::GenerationStopReason::NotStarted {
            reason: super::NotStartedReason::RuntimeRestart,
        } => super::AssistantDiscardReason::RuntimeRestart,
        super::GenerationStopReason::ProjectionFailure { .. } => {
            super::AssistantDiscardReason::ProjectionRepair
        }
        _ => super::AssistantDiscardReason::ProviderFailed,
    };
    let generation_scope = SurfaceScope::Generation {
        fence: fence.clone(),
    };
    if !stream_discards_cover_open_streams(
        state,
        fence,
        &generation_scope,
        expected_discard_reason,
        stream_discards,
    ) {
        return false;
    }
    matches!(
        (&finalization.scope, &finalization.event),
        (
            SurfaceScope::Operation {
                operation_id: scoped_operation,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::FinalizationStarted {
                operation_id: patch_operation,
                finalize_intent_id: patch_intent,
                selected_cause: super::OperationFinalizationCause::GenerationStop(selected_reason),
                suspended_cause: None,
                ..
            }),
        ) if scoped_operation == operation_id
            && patch_operation == operation_id
            && patch_intent == finalize_intent_id
            && selected_reason == stop_reason
    )
}

fn workflow_background_stop_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    background_permit: &SurfacePublisherPermit,
    finalizer_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if provider_background_stop_authorized(
        state,
        issued_permits,
        actor_permit,
        background_permit,
        finalizer_permit,
        batch,
        owner_epoch,
    ) {
        return true;
    }
    if !issued_permits.contains(actor_permit)
        || !issued_permits.contains(background_permit)
        || !issued_permits.contains(finalizer_permit)
    {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Background {
            fence: background_fence,
            ..
        },
        SurfacePublisherPermit::Finalizer {
            operation_id,
            finalize_intent_id,
            owner_epoch: finalizer_epoch,
            ..
        },
    ) = (actor_permit, background_permit, finalizer_permit)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || *finalizer_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
        || operation_id != &background_fence.operation_fence.operation_id
    {
        return false;
    }
    let events = batch.events.as_slice();
    if !matches!(events.len(), 5 | 6) {
        return false;
    }
    let task_event = &events[0];
    let workflow_events = &events[1..events.len() - 3];
    let result_event = &events[events.len() - 3];
    let stop_event = &events[events.len() - 2];
    let finalization_event = &events[events.len() - 1];
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::StatusChanged {
            task_id,
            expected_revision: task_expected,
            next_revision: task_next,
            status: task_status,
            ..
        }),
    ) = (&task_event.scope, &task_event.event)
    else {
        return false;
    };
    let (workflow_fence, terminal_workflow_revision) = match workflow_events {
        [workflow_event] => match (&workflow_event.scope, &workflow_event.event) {
            (
                SurfaceScope::Thread,
                super::SurfaceEvent::Workflow(super::WorkflowPatch::Completed {
                    fence,
                    next_revision,
                }),
            )
            | (
                SurfaceScope::Thread,
                super::SurfaceEvent::Workflow(super::WorkflowPatch::Failed {
                    fence,
                    next_revision,
                    ..
                }),
            )
            | (
                SurfaceScope::Thread,
                super::SurfaceEvent::Workflow(super::WorkflowPatch::Stopped {
                    fence,
                    next_revision,
                    ..
                }),
            )
            | (
                SurfaceScope::Thread,
                super::SurfaceEvent::Workflow(super::WorkflowPatch::Cancelled {
                    fence,
                    next_revision,
                    ..
                }),
            ) => (fence, *next_revision),
            _ => return false,
        },
        [stopping_event, stopped_event] => {
            let (
                SurfaceScope::Thread,
                super::SurfaceEvent::Workflow(super::WorkflowPatch::Stopping {
                    fence,
                    next_revision: stopping_revision,
                    ..
                }),
            ) = (&stopping_event.scope, &stopping_event.event)
            else {
                return false;
            };
            let (
                SurfaceScope::Thread,
                super::SurfaceEvent::Workflow(super::WorkflowPatch::Stopped {
                    fence: stopped_fence,
                    next_revision: stopped_revision,
                    ..
                }),
            ) = (&stopped_event.scope, &stopped_event.event)
            else {
                return false;
            };
            if stopped_fence.workflow_run_id != fence.workflow_run_id
                || stopped_fence.parent != fence.parent
                || stopped_fence.workflow_revision != *stopping_revision
                || stopping_revision.get().checked_add(1) != Some(stopped_revision.get())
            {
                return false;
            }
            (fence, *stopped_revision)
        }
        _ => return false,
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Workflow(super::WorkflowPatch::ResultReady {
            fence: result_fence,
            next_revision: result_next,
            result,
        }),
    ) = (&result_event.scope, &result_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Background { fence: stop_scope },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
            fence: stop_fence,
            reason: stop_reason,
            ..
        }),
    ) = (&stop_event.scope, &stop_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Background {
            fence: finalization_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::FinalizationStarted {
            operation_id: patch_operation,
            finalize_intent_id: patch_intent,
            selected_cause: super::OperationFinalizationCause::GenerationStop(selected_reason),
            suspended_cause: None,
            ..
        }),
    ) = (&finalization_event.scope, &finalization_event.event)
    else {
        return false;
    };
    let snapshot = state.snapshot();
    let Some(task) = snapshot.tasks.iter().find(|task| &task.task_id == task_id) else {
        return false;
    };
    let Some(workflow) = snapshot
        .workflows
        .iter()
        .find(|workflow| workflow.workflow_run_id == workflow_fence.workflow_run_id)
    else {
        return false;
    };
    matches!(
        task_status,
        super::SurfaceTaskStatus::Stopped
            | super::SurfaceTaskStatus::Completed
            | super::SurfaceTaskStatus::Failed
            | super::SurfaceTaskStatus::Cancelled
    ) && task.revision == *task_expected
        && task_expected.get().checked_add(1) == Some(task_next.get())
        && task.background_fence.as_ref() == Some(background_fence)
        && workflow.task_id == *task_id
        && workflow.revision == workflow_fence.workflow_revision
        && terminal_workflow_revision.get().checked_add(1) == Some(result_next.get())
        && result_fence.workflow_run_id == workflow_fence.workflow_run_id
        && result_fence.workflow_revision == terminal_workflow_revision
        && result_fence.parent == workflow_fence.parent
        && result.acknowledged_by_operation.is_none()
        && stop_scope == background_fence
        && finalization_scope == background_fence
        && stop_fence == &background_fence.operation_fence
        && patch_operation == operation_id
        && patch_intent == finalize_intent_id
        && selected_reason == stop_reason
}

#[allow(clippy::too_many_arguments)]
fn provider_background_stop_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    background_permit: &SurfacePublisherPermit,
    finalizer_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit)
        || !issued_permits.contains(background_permit)
        || !issued_permits.contains(finalizer_permit)
    {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Background {
            fence: background_fence,
            ..
        },
        SurfacePublisherPermit::Finalizer {
            operation_id,
            finalize_intent_id,
            owner_epoch: finalizer_epoch,
            ..
        },
    ) = (actor_permit, background_permit, finalizer_permit)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || *finalizer_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
        || operation_id != &background_fence.operation_fence.operation_id
    {
        return false;
    }
    let events = batch.events.as_slice();
    if events.len() < 3 {
        return false;
    }
    let (response_events, terminalization_events) = events.split_at(events.len() - 3);
    let [task_event, stop_event, finalization_event] = terminalization_events else {
        return false;
    };
    let mut completed_response = None;
    let mut tool_requests = Vec::new();
    for event in response_events {
        let SurfaceScope::Background {
            fence: response_scope,
        } = &event.scope
        else {
            return false;
        };
        if response_scope != background_fence {
            return false;
        }
        match &event.event {
            super::SurfaceEvent::Assistant(
                super::AssistantPatch::Delta { .. } | super::AssistantPatch::StreamDiscarded { .. },
            ) if completed_response.is_none() => {}
            super::SurfaceEvent::Assistant(super::AssistantPatch::ResponseCompleted {
                response,
            }) if completed_response.is_none() => {
                completed_response = Some(response);
            }
            super::SurfaceEvent::Tool(super::ToolPatch::Requested { request })
                if completed_response.is_some() =>
            {
                tool_requests.push(request);
            }
            _ => return false,
        }
    }
    if !response_events.is_empty() && completed_response.is_none() {
        return false;
    }
    if let Some(response) = completed_response
        && (response.tool_calls.len() != tool_requests.len()
            || response
                .tool_calls
                .iter()
                .zip(tool_requests.iter())
                .any(|(raw, request)| {
                    raw.id != request.tool_call_id
                        || raw.name != request.name
                        || raw.raw_arguments != request.raw_arguments
                        || raw.arguments_digest != request.arguments_digest
                        || request.source_response_id.as_ref() != Some(&response.response_id)
                        || request.turn_id != response.turn_id
                }))
    {
        return false;
    }
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::StatusChanged {
            task_id,
            expected_revision,
            next_revision,
            status,
            ..
        }),
    ) = (&task_event.scope, &task_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Background { fence: stop_scope },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
            fence: stop_fence,
            reason,
            ..
        }),
    ) = (&stop_event.scope, &stop_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Background {
            fence: finalization_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::FinalizationStarted {
            operation_id: patch_operation,
            finalize_intent_id: patch_intent,
            selected_cause: super::OperationFinalizationCause::GenerationStop(selected_reason),
            suspended_cause: None,
            ..
        }),
    ) = (&finalization_event.scope, &finalization_event.event)
    else {
        return false;
    };
    let snapshot = state.snapshot();
    let Some(background) = snapshot
        .background_operations
        .iter()
        .find(|background| background.fence == *background_fence)
    else {
        return false;
    };
    let Some(task) = snapshot.tasks.iter().find(|task| &task.task_id == task_id) else {
        return false;
    };
    background.operation_id == *operation_id
        && background.task_id.as_ref() == Some(task_id)
        && task.task_type == super::SurfaceTaskType::MainSession
        && task.revision == *expected_revision
        && expected_revision.get().checked_add(1) == Some(next_revision.get())
        && matches!(
            status,
            super::SurfaceTaskStatus::Stopped
                | super::SurfaceTaskStatus::Completed
                | super::SurfaceTaskStatus::Failed
                | super::SurfaceTaskStatus::ApprovalRequired
                | super::SurfaceTaskStatus::Cancelled
        )
        && (*status != super::SurfaceTaskStatus::ApprovalRequired
            || matches!(
                reason,
                super::GenerationStopReason::ExecutionFailed {
                    class: super::GenerationExecutionFailureClass::LegacyApprovalRequired,
                    ..
                }
            ))
        && (task.background_fence.as_ref() == Some(background_fence)
            || (!task.backgrounded && task.background_fence.is_none()))
        && stop_scope == background_fence
        && finalization_scope == background_fence
        && stop_fence == &background_fence.operation_fence
        && patch_operation == operation_id
        && patch_intent == finalize_intent_id
        && selected_reason == reason
}

fn provider_background_suspend_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    background_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(background_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Background {
            fence: background_fence,
            ..
        },
    ) = (actor_permit, background_permit)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let events = batch.events.as_slice();
    if events.len() < 4 {
        return false;
    }
    let (response_events, tail) = events.split_at(events.len() - 4);
    let [task_event, stop_event, interaction_event, suspended_event] = tail else {
        return false;
    };
    let mut completed_response = None;
    let mut tool_requests = Vec::new();
    for event in response_events {
        let SurfaceScope::Background {
            fence: response_scope,
        } = &event.scope
        else {
            return false;
        };
        if response_scope != background_fence {
            return false;
        }
        match &event.event {
            super::SurfaceEvent::Assistant(
                super::AssistantPatch::Delta { .. } | super::AssistantPatch::StreamDiscarded { .. },
            ) if completed_response.is_none() => {}
            super::SurfaceEvent::Assistant(super::AssistantPatch::ResponseCompleted {
                response,
            }) if completed_response.is_none() => {
                completed_response = Some(response);
            }
            super::SurfaceEvent::Tool(super::ToolPatch::Requested { request })
                if completed_response.is_some() =>
            {
                tool_requests.push(request);
            }
            _ => return false,
        }
    }
    let Some(response) = completed_response else {
        return false;
    };
    if response.tool_calls.len() != tool_requests.len()
        || response
            .tool_calls
            .iter()
            .zip(tool_requests.iter())
            .any(|(raw, request)| {
                raw.id != request.tool_call_id
                    || raw.name != request.name
                    || raw.raw_arguments != request.raw_arguments
                    || raw.arguments_digest != request.arguments_digest
                    || request.source_response_id.as_ref() != Some(&response.response_id)
                    || request.turn_id != response.turn_id
            })
    {
        return false;
    }
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::StatusChanged {
            task_id,
            expected_revision,
            next_revision,
            status: super::SurfaceTaskStatus::ApprovalRequired,
            ..
        }),
    ) = (&task_event.scope, &task_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Background { fence: stop_scope },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
            fence: stop_fence,
            reason: super::GenerationStopReason::ProviderSuspended,
            ..
        }),
    ) = (&stop_event.scope, &stop_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Background {
            fence: interaction_scope,
        },
        super::SurfaceEvent::Interaction(super::InteractionPatch::Requested { interaction }),
    ) = (&interaction_event.scope, &interaction_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Background {
            fence: suspended_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::Suspended {
            operation_id: suspended_operation,
            cause: super::SuspensionCause::ProviderSuspended { generation_id },
        }),
    ) = (&suspended_event.scope, &suspended_event.event)
    else {
        return false;
    };
    let snapshot = state.snapshot();
    let Some(background) = snapshot
        .background_operations
        .iter()
        .find(|background| background.fence == *background_fence)
    else {
        return false;
    };
    let Some(task) = snapshot.tasks.iter().find(|task| &task.task_id == task_id) else {
        return false;
    };
    let super::SurfaceInteractionRequest::BackgroundApproval {
        task: task_fence,
        tool,
        ..
    } = &interaction.request
    else {
        return false;
    };
    background.task_id.as_ref() == Some(task_id)
        && background.operation_id == background_fence.operation_fence.operation_id
        && task.task_type == super::SurfaceTaskType::MainSession
        && task.revision == *expected_revision
        && expected_revision.get().checked_add(1) == Some(next_revision.get())
        && task.background_fence.as_ref() == Some(background_fence)
        && task_fence.task_id == *task_id
        && task_fence.task_revision == *next_revision
        && task_fence.background_owner.as_ref() == Some(background_fence)
        && interaction.kind == super::SurfaceInteractionKind::BackgroundApproval
        && interaction.fence == background_fence.operation_fence
        && matches!(
            interaction.route,
            super::SurfaceInteractionRoute::Unassigned { .. }
        )
        && matches!(
            interaction.recovery_disposition,
            super::InteractionUnavailableDisposition::AwaitCapableAttachment { .. }
        )
        && interaction_scope == background_fence
        && tool_requests.len() == 1
        && tool_requests[0] == tool
        && stop_scope == background_fence
        && stop_fence == &background_fence.operation_fence
        && suspended_scope == background_fence
        && suspended_operation == &background_fence.operation_fence.operation_id
        && generation_id == &background_fence.operation_fence.generation_id
}

fn provider_background_interaction_route_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    background_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(background_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Background {
            fence: background_fence,
            ..
        },
    ) = (actor_permit, background_permit)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let [event] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Background { fence: scope },
        super::SurfaceEvent::Interaction(super::InteractionPatch::RouteChanged {
            interaction_id,
            expected_revision,
            next_revision,
            route,
        }),
    ) = (&event.scope, &event.event)
    else {
        return false;
    };
    let Some(interaction) = state
        .snapshot()
        .interactions
        .iter()
        .find(|interaction| &interaction.interaction_id == interaction_id)
    else {
        return false;
    };
    scope == background_fence
        && interaction.fence == background_fence.operation_fence
        && interaction.kind == super::SurfaceInteractionKind::BackgroundApproval
        && interaction.revision == *expected_revision
        && expected_revision.get().checked_add(1) == Some(next_revision.get())
        && match (&interaction.route, route) {
            (
                super::SurfaceInteractionRoute::Unassigned { epoch: current },
                super::SurfaceInteractionRoute::Exclusive { epoch: next, .. }
                | super::SurfaceInteractionRoute::SharedFirstCommitWins { epoch: next, .. },
            ) => current.get().checked_add(1) == Some(next.get()),
            (
                super::SurfaceInteractionRoute::Exclusive { epoch: current, .. }
                | super::SurfaceInteractionRoute::SharedFirstCommitWins { epoch: current, .. },
                super::SurfaceInteractionRoute::Unassigned { epoch: next },
            ) => current.get().checked_add(1) == Some(next.get()),
            _ => false,
        }
}

fn provider_background_interaction_resolution_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    background_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
    expected_safe_projection: Option<&super::SurfaceInteractionSafeProjection>,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(background_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Background {
            fence: background_fence,
            ..
        },
    ) = (actor_permit, background_permit)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let [event] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Background { fence: scope },
        super::SurfaceEvent::Interaction(super::InteractionPatch::Resolved {
            interaction_id,
            expected_revision,
            next_revision,
            receipt,
            ..
        }),
    ) = (&event.scope, &event.event)
    else {
        return false;
    };
    let Some(interaction) = state
        .snapshot()
        .interactions
        .iter()
        .find(|interaction| &interaction.interaction_id == interaction_id)
    else {
        return false;
    };
    scope == background_fence
        && interaction.fence == background_fence.operation_fence
        && interaction.kind == super::SurfaceInteractionKind::BackgroundApproval
        && receipt.kind == super::SurfaceInteractionKind::BackgroundApproval
        && expected_safe_projection.is_none_or(|expected| &receipt.safe_projection == expected)
        && interaction.revision == *expected_revision
        && expected_revision.get().checked_add(1) == Some(next_revision.get())
        && matches!(
            interaction.lifecycle,
            super::SurfaceInteractionLifecycle::Requested
        )
}

fn provider_background_resume_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    background_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(background_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Background {
            fence: background_fence,
            ..
        },
    ) = (actor_permit, background_permit)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let [reservation, control, status_event] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Background {
            fence: reservation_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationReserved { generation }),
    ) = (&reservation.scope, &reservation.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Operation {
            operation_id: control_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
            operation_id,
            request_id,
            intent: super::PendingControlIntent::ResumeStarting { generation_fence },
        }),
    ) = (&control.scope, &control.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::StatusChanged {
            task_id,
            expected_revision,
            next_revision,
            status: super::SurfaceTaskStatus::Running,
            completed_at: None,
            result: None,
            error: None,
        }),
    ) = (&status_event.scope, &status_event.event)
    else {
        return false;
    };
    let snapshot = state.snapshot();
    let Some(operation) = snapshot
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == background_fence.operation_fence.operation_id)
    else {
        return false;
    };
    let Some(background) = snapshot
        .background_operations
        .iter()
        .find(|background| background.fence == *background_fence)
    else {
        return false;
    };
    let Some(task) = snapshot.tasks.iter().find(|task| &task.task_id == task_id) else {
        return false;
    };
    reservation_scope == background_fence
        && generation.predecessor.as_ref() == Some(&background_fence.operation_fence)
        && generation.fence.operation_id == background_fence.operation_fence.operation_id
        && generation.fence.generation_id.get()
            == background_fence
                .operation_fence
                .generation_id
                .get()
                .saturating_add(1)
        && generation.phase == super::GenerationPhase::Reserved
        && control_scope == &operation.operation_id
        && operation_id == &operation.operation_id
        && request_id == &operation.request_id
        && generation_fence == &generation.fence
        && matches!(
            operation.phase,
            super::OperationPhase::Suspended {
                cause: super::SuspensionCause::ProviderSuspended { .. }
            }
        )
        && background.task_id.as_ref() == Some(task_id)
        && task.revision == *expected_revision
        && expected_revision.get().checked_add(1) == Some(next_revision.get())
        && !task.backgrounded
        && task.background_fence.is_none()
}

#[allow(clippy::too_many_arguments)]
fn goal_generation_stop_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    finished_goal_permit: &SurfacePublisherPermit,
    verification_goal_permit: Option<&SurfacePublisherPermit>,
    decision_goal_permit: &SurfacePublisherPermit,
    generation_permit: &SurfacePublisherPermit,
    finalizer_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(finished_goal_permit)
        || verification_goal_permit.is_some_and(|permit| !issued_permits.contains(permit))
        || !issued_permits.contains(decision_goal_permit)
        || !issued_permits.contains(generation_permit)
        || !issued_permits.contains(finalizer_permit)
    {
        return false;
    }
    let (
        SurfacePublisherPermit::Goal {
            goal_fence: finished_fence,
            receipt_digest: finished_digest,
            ..
        },
        SurfacePublisherPermit::Goal {
            goal_fence: decision_fence,
            receipt_digest: decision_digest,
            ..
        },
        SurfacePublisherPermit::Generation { fence, .. },
        SurfacePublisherPermit::Finalizer {
            operation_id,
            finalize_intent_id,
            owner_epoch: finalizer_owner_epoch,
            ..
        },
    ) = (
        finished_goal_permit,
        decision_goal_permit,
        generation_permit,
        finalizer_permit,
    )
    else {
        return false;
    };
    let verification_parts = verification_goal_permit.and_then(|permit| match permit {
        SurfacePublisherPermit::Goal {
            goal_fence,
            receipt_digest,
            ..
        } => Some((goal_fence, receipt_digest)),
        _ => None,
    });
    if verification_goal_permit.is_some() != verification_parts.is_some()
        || *finalizer_owner_epoch != owner_epoch
        || operation_id != &fence.operation_id
        || finished_fence.goal_id != decision_fence.goal_id
        || match verification_parts {
            Some((verification_fence, _)) => {
                finished_fence.goal_id != verification_fence.goal_id
                    || finished_fence.goal_revision.get().checked_add(1)
                        != Some(verification_fence.goal_revision.get())
                    || verification_fence.goal_revision.get().checked_add(1)
                        != Some(decision_fence.goal_revision.get())
            }
            None => {
                finished_fence.goal_revision.get().checked_add(1)
                    != Some(decision_fence.goal_revision.get())
            }
        }
    {
        return false;
    }
    let events = batch.events.as_slice();
    if events.len() < 4 {
        return false;
    }
    let goal_event_count = if verification_parts.is_some() { 3 } else { 2 };
    let (core_events, goal_events) = events.split_at(events.len() - goal_event_count);
    let (finished_event, verification_event, decision_event) = match goal_events {
        [finished, decision] => (finished, None, decision),
        [finished, verification, decision] => (finished, Some(verification), decision),
        _ => return false,
    };
    let goal_event_matches = |event: &super::SurfaceEventEnvelope,
                              goal_fence: &super::SurfaceGoalFence,
                              digest: &super::Sha256Digest| {
        matches!(
            (&event.scope, &event.event),
            (
                SurfaceScope::Goal {
                    goal_id,
                    causative_generation: Some(causative),
                },
                super::SurfaceEvent::Goal(envelope),
            ) if goal_id == &goal_fence.goal_id
                && causative == fence
                && envelope.receipt.goal_id == goal_fence.goal_id
                && envelope.receipt.goal_revision == goal_fence.goal_revision
                && envelope.receipt.goal_owner_epoch == goal_fence.goal_owner_epoch
                && envelope.receipt.receipt_digest == *digest
        )
    };
    if !goal_event_matches(finished_event, finished_fence, finished_digest)
        || match (verification_event, verification_parts) {
            (Some(event), Some((verification_fence, verification_digest))) => {
                !goal_event_matches(event, verification_fence, verification_digest)
            }
            (None, None) => false,
            _ => true,
        }
        || !goal_event_matches(decision_event, decision_fence, decision_digest)
    {
        return false;
    }
    let super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
        patch: super::GoalPatch::OuterTurnFinished { identity, .. },
        ..
    }) = &finished_event.event
    else {
        return false;
    };
    let super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
        patch:
            super::GoalPatch::ContinuationDecided {
                predecessor,
                decision: super::GoalContinuationDecision::Stopped { .. },
                ..
            },
        ..
    }) = &decision_event.event
    else {
        return false;
    };
    if let Some(event) = verification_event
        && !matches!(
            &event.event,
            super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                patch: super::GoalPatch::VerificationCompleted {
                    identity: verification_identity,
                    ..
                },
                ..
            }) if verification_identity == identity
        )
    {
        return false;
    }
    if identity != predecessor || &identity.operation_fence != fence {
        return false;
    }
    let Some((finalization, prefix)) = core_events.split_last() else {
        return false;
    };
    let Some((stop, stream_discards)) = prefix.split_last() else {
        return false;
    };
    let stop_reason = match (&stop.scope, &stop.event) {
        (
            SurfaceScope::Generation { fence: scope },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                reason,
                ..
            }),
        ) if scope == fence && patch_fence == fence => reason,
        _ => return false,
    };
    let expected_discard_reason = match stop_reason {
        super::GenerationStopReason::Cancelled { .. } => {
            super::AssistantDiscardReason::GenerationCancelled
        }
        super::GenerationStopReason::InterruptedResumable => {
            super::AssistantDiscardReason::GenerationInterrupted
        }
        super::GenerationStopReason::RuntimeRestart
        | super::GenerationStopReason::NotStarted {
            reason: super::NotStartedReason::RuntimeRestart,
        } => super::AssistantDiscardReason::RuntimeRestart,
        super::GenerationStopReason::ProjectionFailure { .. } => {
            super::AssistantDiscardReason::ProjectionRepair
        }
        _ => super::AssistantDiscardReason::ProviderFailed,
    };
    if !stream_discards_cover_open_streams(
        state,
        fence,
        &SurfaceScope::Generation {
            fence: fence.clone(),
        },
        expected_discard_reason,
        stream_discards,
    ) {
        return false;
    }
    matches!(
        (&finalization.scope, &finalization.event),
        (
            SurfaceScope::Operation {
                operation_id: scoped_operation,
            },
            super::SurfaceEvent::Operation(super::OperationPatch::FinalizationStarted {
                operation_id: patch_operation,
                finalize_intent_id: patch_intent,
                selected_cause: super::OperationFinalizationCause::GenerationStop(selected_reason),
                suspended_cause: None,
                ..
            }),
        ) if scoped_operation == operation_id
            && patch_operation == operation_id
            && patch_intent == finalize_intent_id
            && selected_reason == stop_reason
    )
}

#[allow(clippy::too_many_arguments)]
fn goal_generation_continue_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    finished_goal_permit: &SurfacePublisherPermit,
    verification_goal_permit: Option<&SurfacePublisherPermit>,
    decision_goal_permit: &SurfacePublisherPermit,
    predecessor_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit)
        || !issued_permits.contains(finished_goal_permit)
        || verification_goal_permit.is_some_and(|permit| !issued_permits.contains(permit))
        || !issued_permits.contains(decision_goal_permit)
        || !issued_permits.contains(predecessor_permit)
    {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Goal {
            goal_fence: finished_fence,
            receipt_digest: finished_digest,
            ..
        },
        SurfacePublisherPermit::Goal {
            goal_fence: decision_fence,
            receipt_digest: decision_digest,
            ..
        },
        SurfacePublisherPermit::Generation {
            fence: predecessor_fence,
            ..
        },
    ) = (
        actor_permit,
        finished_goal_permit,
        decision_goal_permit,
        predecessor_permit,
    )
    else {
        return false;
    };
    let verification_parts = verification_goal_permit.and_then(|permit| match permit {
        SurfacePublisherPermit::Goal {
            goal_fence,
            receipt_digest,
            ..
        } => Some((goal_fence, receipt_digest)),
        _ => None,
    });
    if verification_goal_permit.is_some() != verification_parts.is_some()
        || *actor_epoch != owner_epoch
        || thread_id != &predecessor_fence.thread_id
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
        || finished_fence.goal_id != decision_fence.goal_id
        || match verification_parts {
            Some((verification_fence, _)) => {
                finished_fence.goal_id != verification_fence.goal_id
                    || finished_fence.goal_revision.get().checked_add(1)
                        != Some(verification_fence.goal_revision.get())
                    || verification_fence.goal_revision.get().checked_add(1)
                        != Some(decision_fence.goal_revision.get())
            }
            None => {
                finished_fence.goal_revision.get().checked_add(1)
                    != Some(decision_fence.goal_revision.get())
            }
        }
    {
        return false;
    }
    let goal_event_count = if verification_parts.is_some() { 3 } else { 2 };
    let events = batch.events.as_slice();
    if events.len() < goal_event_count + 2 {
        return false;
    }
    let (core_events, goal_events) = events.split_at(events.len() - goal_event_count);
    let (finished_event, verification_event, decision_event) = match goal_events {
        [finished, decision] => (finished, None, decision),
        [finished, verification, decision] => (finished, Some(verification), decision),
        _ => return false,
    };
    let goal_event_matches = |event: &super::SurfaceEventEnvelope,
                              goal_fence: &super::SurfaceGoalFence,
                              digest: &super::Sha256Digest| {
        matches!(
            (&event.scope, &event.event),
            (
                SurfaceScope::Goal {
                    goal_id,
                    causative_generation: Some(causative),
                },
                super::SurfaceEvent::Goal(envelope),
            ) if goal_id == &goal_fence.goal_id
                && causative == predecessor_fence
                && envelope.receipt.goal_id == goal_fence.goal_id
                && envelope.receipt.goal_revision == goal_fence.goal_revision
                && envelope.receipt.goal_owner_epoch == goal_fence.goal_owner_epoch
                && envelope.receipt.receipt_digest == *digest
        )
    };
    if !goal_event_matches(finished_event, finished_fence, finished_digest)
        || match (verification_event, verification_parts) {
            (Some(event), Some((verification_fence, verification_digest))) => {
                !goal_event_matches(event, verification_fence, verification_digest)
            }
            (None, None) => false,
            _ => true,
        }
        || !goal_event_matches(decision_event, decision_fence, decision_digest)
    {
        return false;
    }
    let super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
        patch: super::GoalPatch::OuterTurnFinished { identity, .. },
        ..
    }) = &finished_event.event
    else {
        return false;
    };
    if let Some(event) = verification_event
        && !matches!(
            &event.event,
            super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
                patch: super::GoalPatch::VerificationCompleted {
                    identity: verification_identity,
                    ..
                },
                ..
            }) if verification_identity == identity
        )
    {
        return false;
    }
    let successor = match &decision_event.event {
        super::SurfaceEvent::Goal(super::GoalPatchEnvelope {
            patch:
                super::GoalPatch::ContinuationDecided {
                    predecessor,
                    decision: super::GoalContinuationDecision::Admitted { successor, .. },
                    ..
                },
            ..
        }) if predecessor == identity => successor,
        _ => return false,
    };
    if &identity.operation_fence != predecessor_fence {
        return false;
    }
    let Some((input_item, prefix)) = core_events.split_last() else {
        return false;
    };
    let Some((reserved, prefix)) = prefix.split_last() else {
        return false;
    };
    let Some((stopped, stream_discards)) = prefix.split_last() else {
        return false;
    };
    if !matches!(
        (&stopped.scope, &stopped.event),
        (
            SurfaceScope::Generation { fence: scope },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence,
                reason: super::GenerationStopReason::Completed {
                    status:
                        super::GenerationCompletionStatus::Success
                        | super::GenerationCompletionStatus::BudgetExhausted {
                            budget:
                                super::OperationBudget::TurnRequests {
                                    scope: super::TurnRequestBudgetScope::AgentLoop,
                                    ..
                                },
                        },
                },
                ..
            }),
        ) if scope == predecessor_fence && fence == predecessor_fence
    ) || !matches!(
        (&reserved.scope, &reserved.event),
        (
            SurfaceScope::Generation { fence: scope },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationReserved {
                generation,
            }),
        ) if scope == &successor.operation_fence
            && generation.fence == successor.operation_fence
            && generation.predecessor.as_ref() == Some(predecessor_fence)
            && generation.goal_identity.as_ref() == Some(successor)
            && generation.phase == super::GenerationPhase::Reserved
    ) || !matches!(
        (&input_item.scope, &input_item.event),
        (
            SurfaceScope::Generation { fence },
            super::SurfaceEvent::Item(super::ItemPatch::Added {
                item: super::SurfaceItem::UserMessage {
                    id,
                    turn_id,
                    input: super::SurfaceUserInputState::Pending { .. },
                    origin: super::SurfaceItemOrigin::GoalContinuation,
                    ..
                },
            }),
        ) if fence == &successor.operation_fence
            && id == &successor.canonical_input_item_id
            && turn_id == &successor.logical_turn_id
    ) {
        return false;
    }
    stream_discards_cover_open_streams(
        state,
        predecessor_fence,
        &SurfaceScope::Generation {
            fence: predecessor_fence.clone(),
        },
        super::AssistantDiscardReason::ProviderFailed,
        stream_discards,
    )
}

fn actor_background_control_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    background_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if actor_provider_background_control_authorized(
        state,
        issued_permits,
        actor_permit,
        background_permit,
        batch,
        owner_epoch,
    ) {
        return true;
    }
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(background_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Background {
            fence: background_fence,
            ..
        },
    ) = (actor_permit, background_permit)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let [control_event, task_event, workflow_event] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Background {
            fence: control_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
            operation_id,
            request_id,
            intent:
                super::PendingControlIntent::Terminalize {
                    operation_id: intent_operation,
                    cause: super::TerminalizationCause::UserCancel,
                },
        }),
    ) = (&control_event.scope, &control_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::StatusChanged {
            task_id,
            expected_revision: task_expected,
            next_revision: task_next,
            status: super::SurfaceTaskStatus::Stopping,
            completed_at: None,
            result: None,
            error: None,
        }),
    ) = (&task_event.scope, &task_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Workflow(super::WorkflowPatch::Stopping {
            fence: workflow_fence,
            next_revision: workflow_next,
            ..
        }),
    ) = (&workflow_event.scope, &workflow_event.event)
    else {
        return false;
    };
    let snapshot = state.snapshot();
    let Some(background) = snapshot
        .background_operations
        .iter()
        .find(|background| background.operation_id == *operation_id)
    else {
        return false;
    };
    let Some(operation) = snapshot
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == *operation_id && operation.terminal.is_none())
    else {
        return false;
    };
    let Some(task) = snapshot.tasks.iter().find(|task| task.task_id == *task_id) else {
        return false;
    };
    let Some(workflow) = snapshot
        .workflows
        .iter()
        .find(|workflow| workflow.workflow_run_id == workflow_fence.workflow_run_id)
    else {
        return false;
    };
    control_scope == background_fence
        && &background.fence == background_fence
        && intent_operation == operation_id
        && operation.request_id == *request_id
        && operation.pending_control.is_none()
        && background.task_id.as_ref() == Some(task_id)
        && task.parent_operation.as_ref() == Some(operation_id)
        && task.background_fence.as_ref() == Some(background_fence)
        && task.workflow_run_id.as_ref() == Some(&workflow_fence.workflow_run_id)
        && task.revision == *task_expected
        && task_expected.get().checked_add(1) == Some(task_next.get())
        && matches!(
            task.status,
            super::SurfaceTaskStatus::Queued
                | super::SurfaceTaskStatus::Running
                | super::SurfaceTaskStatus::Paused
                | super::SurfaceTaskStatus::ApprovalRequired
        )
        && workflow.task_id == *task_id
        && workflow.revision == workflow_fence.workflow_revision
        && workflow.parent == workflow_fence.parent
        && workflow_fence.workflow_revision.get().checked_add(1) == Some(workflow_next.get())
        && matches!(
            workflow.status,
            super::SurfaceWorkflowStatus::Running
                | super::SurfaceWorkflowStatus::Paused
                | super::SurfaceWorkflowStatus::AsyncLaunched
        )
}

fn actor_provider_background_control_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    background_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(background_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Background {
            fence: background_fence,
            ..
        },
    ) = (actor_permit, background_permit)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let [control_event, task_event] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Background {
            fence: control_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
            operation_id,
            request_id,
            intent:
                super::PendingControlIntent::Terminalize {
                    operation_id: intent_operation,
                    cause: super::TerminalizationCause::UserCancel,
                },
        }),
    ) = (&control_event.scope, &control_event.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::StatusChanged {
            task_id,
            expected_revision,
            next_revision,
            status: super::SurfaceTaskStatus::Stopping,
            completed_at: None,
            result: None,
            error: None,
        }),
    ) = (&task_event.scope, &task_event.event)
    else {
        return false;
    };
    let snapshot = state.snapshot();
    let Some(background) = snapshot
        .background_operations
        .iter()
        .find(|background| background.operation_id == *operation_id)
    else {
        return false;
    };
    let Some(operation) = snapshot
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == *operation_id && operation.terminal.is_none())
    else {
        return false;
    };
    let Some(task) = snapshot.tasks.iter().find(|task| task.task_id == *task_id) else {
        return false;
    };
    let foreground_claimed_denial = !task.backgrounded
        && task.background_fence.is_none()
        && snapshot.interactions.iter().any(|interaction| {
            interaction.fence.operation_id == *operation_id
                && matches!(
                    (
                        &interaction.request,
                        &interaction.lifecycle,
                    ),
                    (
                        super::SurfaceInteractionRequest::BackgroundApproval {
                            task: requested_task,
                            ..
                        },
                        super::SurfaceInteractionLifecycle::Resolved { receipt },
                    ) if requested_task.task_id == *task_id
                        && requested_task.background_owner.as_ref() == Some(background_fence)
                        && requested_task
                            .task_revision
                            .get()
                            .checked_add(1)
                            == Some(task.revision.get())
                        && matches!(
                            &receipt.safe_projection,
                            super::SurfaceInteractionSafeProjection::BackgroundApproval {
                                allowed: false,
                            }
                        )
                )
        });
    control_scope == background_fence
        && &background.fence == background_fence
        && intent_operation == operation_id
        && operation.request_id == *request_id
        && operation.pending_control.is_none()
        && background.task_id.as_ref() == Some(task_id)
        && task.task_type == super::SurfaceTaskType::MainSession
        && task.parent_operation.as_ref() == Some(operation_id)
        && (task.background_fence.as_ref() == Some(background_fence) || foreground_claimed_denial)
        && task.workflow_run_id.is_none()
        && task.revision == *expected_revision
        && expected_revision.get().checked_add(1) == Some(next_revision.get())
        && matches!(
            task.status,
            super::SurfaceTaskStatus::Running
                | super::SurfaceTaskStatus::Paused
                | super::SurfaceTaskStatus::ApprovalRequired
        )
}

fn actor_control_workflow_launch_authorized(batch: &SurfaceCommitBatch) -> bool {
    let [
        requested,
        admitted,
        started,
        task,
        workflow_started,
        async_launched,
        transferred,
    ] = batch.events.as_slice()
    else {
        return false;
    };
    let (
        SurfaceScope::Operation {
            operation_id: requested_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::Requested { operation }),
    ) = (&requested.scope, &requested.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Operation {
            operation_id: admitted_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::Admitted {
            operation_id,
            logical_turn_id,
            input: super::AdmittedInput::NotApplicable,
            first_generation,
        }),
    ) = (&admitted.scope, &admitted.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Generation {
            fence: started_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationStarted {
            fence: started_fence,
            ..
        }),
    ) = (&started.scope, &started.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::Upserted {
            expected_revision: None,
            task: surface_task,
        }),
    ) = (&task.scope, &task.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Workflow(super::WorkflowPatch::Started { workflow }),
    ) = (&workflow_started.scope, &workflow_started.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Workflow(super::WorkflowPatch::AsyncLaunched {
            fence: workflow_fence,
            next_revision,
        }),
    ) = (&async_launched.scope, &async_launched.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Generation {
            fence: transferred_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationTransferred {
            fence: transferred_fence,
            background_fence,
            task_id,
        }),
    ) = (&transferred.scope, &transferred.event)
    else {
        return false;
    };
    requested_scope == &operation.operation_id
        && admitted_scope == requested_scope
        && operation_id == requested_scope
        && matches!(
            operation.intent.kind,
            super::OperationKind::StandaloneWorkflow { .. }
        )
        && first_generation.logical_turn_id == *logical_turn_id
        && started_scope == &first_generation.fence
        && started_fence == started_scope
        && transferred_scope == started_scope
        && transferred_fence == started_scope
        && background_fence.operation_fence == *started_scope
        && task_id.as_ref() == Some(&surface_task.task_id)
        && surface_task.parent_operation.as_ref() == Some(requested_scope)
        && surface_task.background_fence.as_ref() == Some(background_fence)
        && surface_task.workflow_run_id.as_ref() == Some(&workflow.workflow_run_id)
        && workflow.task_id == surface_task.task_id
        && workflow.revision.get() == 1
        && workflow_fence.workflow_run_id == workflow.workflow_run_id
        && workflow_fence.workflow_revision == workflow.revision
        && workflow_fence.parent == workflow.parent
        && next_revision.get() == 2
}

fn actor_control_main_session_transfer_authorized(batch: &SurfaceCommitBatch) -> bool {
    let [transfer, task] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Generation {
            fence: transfer_scope,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationTransferred {
            fence,
            background_fence,
            task_id,
        }),
    ) = (&transfer.scope, &transfer.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::Upserted {
            expected_revision: None,
            task,
        }),
    ) = (&task.scope, &task.event)
    else {
        return false;
    };
    transfer_scope == fence
        && background_fence.operation_fence == *fence
        && task_id.as_ref() == Some(&task.task_id)
        && task.revision.get() == 1
        && task.task_type == super::SurfaceTaskType::MainSession
        && task.status == super::SurfaceTaskStatus::Running
        && task.backgrounded
        && task.parent_operation.as_ref() == Some(&fence.operation_id)
        && task.background_fence.as_ref() == Some(background_fence)
        && task.workflow_run_id.is_none()
        && task.subagent_id.is_none()
        && task.pending_interaction_id.is_none()
        && task.completed_at.is_none()
        && task.result.is_none()
        && task.error.is_none()
}

fn actor_control_admission_pair_authorized(batch: &SurfaceCommitBatch) -> bool {
    let [admission, item] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Operation {
            operation_id: scoped_operation,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::Admitted {
            operation_id,
            logical_turn_id,
            input:
                super::AdmittedInput::PendingUser {
                    item_id,
                    presentation,
                    correlation_id,
                },
            first_generation,
        }),
    ) = (&admission.scope, &admission.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Generation { fence },
        super::SurfaceEvent::Item(super::ItemPatch::Added {
            item:
                super::SurfaceItem::UserMessage {
                    id,
                    turn_id,
                    input:
                        super::SurfaceUserInputState::Pending {
                            presentation: item_presentation,
                            correlation_id: item_correlation,
                        },
                    pinned: _,
                    origin: super::SurfaceItemOrigin::UserInput,
                },
        }),
    ) = (&item.scope, &item.event)
    else {
        return false;
    };
    scoped_operation == operation_id
        && fence == &first_generation.fence
        && id == item_id
        && turn_id == logical_turn_id
        && item_presentation == presentation
        && item_correlation == correlation_id
}

fn actor_control_resume_pair_authorized(batch: &SurfaceCommitBatch) -> bool {
    let [reserved, resume] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Generation { fence },
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationReserved { generation }),
    ) = (&reserved.scope, &reserved.event)
    else {
        return false;
    };
    let (
        SurfaceScope::Operation {
            operation_id: scoped_operation,
        },
        super::SurfaceEvent::Operation(super::OperationPatch::ControlIntentCommitted {
            operation_id,
            intent: super::PendingControlIntent::ResumeStarting { generation_fence },
            ..
        }),
    ) = (&resume.scope, &resume.event)
    else {
        return false;
    };
    fence == &generation.fence
        && scoped_operation == operation_id
        && operation_id == &fence.operation_id
        && generation_fence == fence
}

fn finalizer_event_authorized(
    operation_id: &super::SurfaceOperationId,
    finalize_intent_id: &super::SurfaceFinalizeIntentId,
    event: &super::SurfaceEventEnvelope,
) -> bool {
    let scope_matches = match &event.scope {
        SurfaceScope::Operation {
            operation_id: scope,
        } => scope == operation_id,
        SurfaceScope::Background { fence } => fence.operation_fence.operation_id == *operation_id,
        _ => false,
    };
    scope_matches
        && matches!(
            &event.event,
            super::SurfaceEvent::Operation(
                super::OperationPatch::FinalizationStarted {
                    operation_id: patch_operation,
                    finalize_intent_id: patch_intent,
                    ..
                }
                    | super::OperationPatch::FinalizationSettlementRecorded {
                        operation_id: patch_operation,
                        finalize_intent_id: patch_intent,
                        ..
                    }
                    | super::OperationPatch::FinalizationDegraded {
                        operation_id: patch_operation,
                        finalize_intent_id: patch_intent,
                        ..
                    }
                    | super::OperationPatch::Terminal {
                        record: super::OperationTerminalRecord {
                            operation_id: patch_operation,
                            finalize_intent_id: patch_intent,
                            ..
                        },
                    }
            ) if patch_operation == operation_id && patch_intent == finalize_intent_id
        )
}

fn actor_generation_interrupt_authorized(
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    generation_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(generation_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_owner_epoch,
            ..
        },
        SurfacePublisherPermit::Generation { fence, .. },
    ) = (actor_permit, generation_permit)
    else {
        return false;
    };
    if *actor_owner_epoch != owner_epoch
        || thread_id != &fence.thread_id
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let Some((intent, cancellations)) = batch.events.as_slice().split_first() else {
        return false;
    };
    if !matches!(
        (&intent.scope, &intent.event),
        (
            SurfaceScope::Operation {
                operation_id: scoped_operation_id,
            },
            super::SurfaceEvent::Operation(
                super::OperationPatch::ControlIntentCommitted {
                    operation_id,
                    intent:
                        super::PendingControlIntent::Interrupt {
                            generation_fence,
                        },
                    ..
                },
            ),
        ) if scoped_operation_id == &fence.operation_id
            && operation_id == &fence.operation_id
            && generation_fence == fence
    ) {
        return false;
    }
    cancellations.iter().all(|event| {
        matches!(
            (&event.scope, &event.event),
            (
                SurfaceScope::Generation { fence: scoped_fence },
                super::SurfaceEvent::Interaction(super::InteractionPatch::Cancelled {
                    reason:
                        super::InteractionCancelReason::OperationCancelled {
                            reason: super::CancelReason::User,
                        },
                    ..
                }),
            ) if scoped_fence == fence
        )
    })
}

fn actor_finalizer_task_terminal_authorized(
    state: &SurfaceReducerState,
    issued_permits: &[SurfacePublisherPermit],
    actor_permit: &SurfacePublisherPermit,
    finalizer_permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
    owner_epoch: ThreadOwnerEpoch,
) -> bool {
    if !issued_permits.contains(actor_permit) || !issued_permits.contains(finalizer_permit) {
        return false;
    }
    let (
        SurfacePublisherPermit::ActorControl {
            thread_id,
            owner_epoch: actor_epoch,
            ..
        },
        SurfacePublisherPermit::Finalizer {
            operation_id,
            finalize_intent_id,
            owner_epoch: finalizer_epoch,
            ..
        },
    ) = (actor_permit, finalizer_permit)
    else {
        return false;
    };
    if *actor_epoch != owner_epoch
        || *finalizer_epoch != owner_epoch
        || thread_id != &batch.cursor_before.thread_id
        || thread_id != &batch.cursor_after.thread_id
    {
        return false;
    }
    let [task_event, terminal_event] = batch.events.as_slice() else {
        return false;
    };
    let (
        SurfaceScope::Thread,
        super::SurfaceEvent::Task(super::TaskPatch::StatusChanged {
            task_id,
            status,
            completed_at: Some(_),
            ..
        }),
    ) = (&task_event.scope, &task_event.event)
    else {
        return false;
    };
    let super::SurfaceEvent::Operation(super::OperationPatch::Terminal { record }) =
        &terminal_event.event
    else {
        return false;
    };
    let Some(task) = state
        .snapshot()
        .tasks
        .iter()
        .find(|task| &task.task_id == task_id)
    else {
        return false;
    };
    let expected_status = match record.terminal {
        super::OperationTerminal::Succeeded { .. } => super::SurfaceTaskStatus::Completed,
        super::OperationTerminal::Cancelled { .. }
        | super::OperationTerminal::NotAdmitted { .. } => super::SurfaceTaskStatus::Cancelled,
        super::OperationTerminal::Shutdown { .. }
        | super::OperationTerminal::AbortedByRuntimeRestart { .. } => {
            super::SurfaceTaskStatus::Stopped
        }
        super::OperationTerminal::Failed { .. }
        | super::OperationTerminal::Panicked { .. }
        | super::OperationTerminal::JoinFailed { .. }
        | super::OperationTerminal::BudgetExhausted { .. } => super::SurfaceTaskStatus::Failed,
    };
    task.task_type == super::SurfaceTaskType::MainSession
        && task.parent_operation.as_ref() == Some(operation_id)
        && record.operation_id == *operation_id
        && record.finalize_intent_id == *finalize_intent_id
        && *status == expected_status
        && finalizer_event_authorized(operation_id, finalize_intent_id, terminal_event)
        && finalizer_background_scope_matches_state(state, finalizer_permit, batch)
}

fn finalizer_background_scope_matches_state(
    state: &SurfaceReducerState,
    permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
) -> bool {
    let SurfacePublisherPermit::Finalizer { operation_id, .. } = permit else {
        return true;
    };
    let expected = state
        .snapshot()
        .background_operations
        .iter()
        .find(|operation| &operation.operation_id == operation_id)
        .map(|operation| &operation.fence);
    batch
        .events
        .as_slice()
        .iter()
        .all(|event| match &event.scope {
            SurfaceScope::Background { fence } => expected == Some(fence),
            _ => true,
        })
}

fn stream_discards_cover_open_streams<'a>(
    state: &SurfaceReducerState,
    fence: &super::SurfaceOperationFence,
    expected_scope: &SurfaceScope,
    expected_reason: super::AssistantDiscardReason,
    events: impl IntoIterator<Item = &'a super::SurfaceEventEnvelope>,
) -> bool {
    let expected = state
        .snapshot()
        .assistant_streams
        .iter()
        .filter(|stream| {
            stream.fence == *fence && stream.state == super::SurfaceAssistantStreamState::Open
        })
        .map(|stream| &stream.stream_id)
        .collect::<Vec<_>>();
    let actual = events.into_iter().collect::<Vec<_>>();
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(event, stream_id)| {
            event.scope == *expected_scope
                && matches!(
                    &event.event,
                    super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded {
                        stream_id: discarded,
                        reason,
                    }) if discarded == stream_id && *reason == expected_reason
                )
        })
}

fn recovery_stream_dispositions_match_state(
    state: &SurfaceReducerState,
    permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
) -> bool {
    let SurfacePublisherPermit::Recovery {
        historical_fence, ..
    } = permit
    else {
        return true;
    };
    let Some(stop) = batch
        .events
        .as_slice()
        .iter()
        .find(|event| recovery_generation_stop_authorized(historical_fence, event))
    else {
        return true;
    };
    let stop_reason = match &stop.event {
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
            reason, ..
        }) => reason,
        _ => return false,
    };
    let expected_reason = match stop_reason {
        super::GenerationStopReason::Cancelled { .. } => {
            super::AssistantDiscardReason::GenerationCancelled
        }
        super::GenerationStopReason::InterruptedResumable => {
            super::AssistantDiscardReason::GenerationInterrupted
        }
        super::GenerationStopReason::RuntimeRestart
        | super::GenerationStopReason::NotStarted {
            reason: super::NotStartedReason::RuntimeRestart,
        } => super::AssistantDiscardReason::RuntimeRestart,
        super::GenerationStopReason::ProjectionFailure { .. } => {
            super::AssistantDiscardReason::ProjectionRepair
        }
        _ => super::AssistantDiscardReason::ProviderFailed,
    };
    let expected_scope = stop.scope.clone();
    stream_discards_cover_open_streams(
        state,
        historical_fence,
        &expected_scope,
        expected_reason,
        batch.events.as_slice().iter().filter(|event| {
            matches!(
                event.event,
                super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded { .. })
            )
        }),
    )
}

fn recovery_batch_authorized(
    historical_fence: &super::SurfaceOperationFence,
    batch: &SurfaceCommitBatch,
) -> bool {
    let events = batch.events.as_slice();
    if recovery_terminal_cleanup_ambiguity_authorized(historical_fence, events) {
        return true;
    }
    if recovery_manual_compaction_completion_authorized(historical_fence, events) {
        return true;
    }
    if let [completed, item] = events
        && let (
            SurfaceScope::Generation {
                fence: completed_fence,
            },
            super::SurfaceEvent::Tool(super::ToolPatch::Completed { result }),
        ) = (&completed.scope, &completed.event)
        && ambiguous_tool_completion_authorized(
            historical_fence,
            events,
            0,
            &result.tool_call_id,
            &mut BTreeSet::new(),
        ) == Some(2)
        && completed_fence == historical_fence
    {
        let _ = item;
        return true;
    }
    if let [capability, lease_event] = events
        && let (
            SurfaceScope::Generation {
                fence: capability_fence,
            },
            super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                call:
                    super::SurfaceCapabilityCall {
                        call_id,
                        fence: call_fence,
                        kind: super::SurfaceCapabilityCallKind::TerminalCreate,
                        owning_tool_call_id,
                        state:
                            super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                effect_kind: super::ExternalEffectKind::TerminalCreate,
                                ..
                            },
                        ..
                    },
            }),
        ) = (&capability.scope, &capability.event)
        && let (
            SurfaceScope::Generation { fence: lease_fence },
            super::SurfaceEvent::Tool(super::ToolPatch::RemoteTerminalLeaseChanged {
                lease:
                    super::SurfaceRemoteTerminalLease {
                        owning_tool_call_id: lease_tool_call_id,
                        state:
                            super::SurfaceRemoteTerminalLeaseState::IdentityUnknown { create_call_id },
                        ..
                    },
            }),
        ) = (&lease_event.scope, &lease_event.event)
        && capability_fence == historical_fence
        && call_fence == historical_fence
        && lease_fence == historical_fence
        && create_call_id == call_id
        && lease_tool_call_id == owning_tool_call_id
    {
        return true;
    }
    if let [capability, lease_event, completed, item] = events
        && let (
            SurfaceScope::Generation {
                fence: capability_fence,
            },
            super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                call:
                    super::SurfaceCapabilityCall {
                        call_id,
                        kind: super::SurfaceCapabilityCallKind::TerminalCreate,
                        owning_tool_call_id,
                        state:
                            super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                effect_kind: super::ExternalEffectKind::TerminalCreate,
                                ..
                            },
                        ..
                    },
            }),
        ) = (&capability.scope, &capability.event)
        && let (
            SurfaceScope::Generation { fence: lease_fence },
            super::SurfaceEvent::Tool(super::ToolPatch::RemoteTerminalLeaseChanged {
                lease:
                    super::SurfaceRemoteTerminalLease {
                        owning_tool_call_id: lease_tool_call_id,
                        state:
                            super::SurfaceRemoteTerminalLeaseState::IdentityUnknown { create_call_id },
                        ..
                    },
            }),
        ) = (&lease_event.scope, &lease_event.event)
        && let (
            SurfaceScope::Generation {
                fence: completed_fence,
            },
            super::SurfaceEvent::Tool(super::ToolPatch::Completed { result }),
        ) = (&completed.scope, &completed.event)
        && let (
            SurfaceScope::Generation { fence: item_fence },
            super::SurfaceEvent::Item(super::ItemPatch::Added {
                item:
                    super::SurfaceItem::ToolResultMessage {
                        tool_call_id,
                        terminal,
                        ..
                    },
            }),
        ) = (&item.scope, &item.event)
        && capability_fence == historical_fence
        && lease_fence == historical_fence
        && completed_fence == historical_fence
        && item_fence == historical_fence
        && create_call_id == call_id
        && lease_tool_call_id == owning_tool_call_id
        && &result.tool_call_id == owning_tool_call_id
        && tool_call_id == owning_tool_call_id
        && result.terminal == *terminal
        && matches!(
            result.terminal,
            super::SurfaceToolTerminal {
                kind: super::SurfaceToolResultKind::ExternalEffectAmbiguous,
                source: super::ToolTerminalSource::Observed,
                invocation_started: super::ToolInvocationStarted::Yes,
            }
        )
    {
        return true;
    }
    if let [capability, completed, item] = events
        && let (
            SurfaceScope::Generation {
                fence: capability_fence,
            },
            super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                call:
                    super::SurfaceCapabilityCall {
                        kind: super::SurfaceCapabilityCallKind::WriteTextFile,
                        owning_tool_call_id,
                        state:
                            super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                effect_kind: super::ExternalEffectKind::FileWrite,
                                ..
                            },
                        ..
                    },
            }),
        ) = (&capability.scope, &capability.event)
        && let (
            SurfaceScope::Generation {
                fence: completed_fence,
            },
            super::SurfaceEvent::Tool(super::ToolPatch::Completed { result }),
        ) = (&completed.scope, &completed.event)
        && let (
            SurfaceScope::Generation { fence: item_fence },
            super::SurfaceEvent::Item(super::ItemPatch::Added {
                item:
                    super::SurfaceItem::ToolResultMessage {
                        tool_call_id,
                        terminal,
                        ..
                    },
            }),
        ) = (&item.scope, &item.event)
        && capability_fence == historical_fence
        && completed_fence == historical_fence
        && item_fence == historical_fence
        && &result.tool_call_id == owning_tool_call_id
        && tool_call_id == owning_tool_call_id
        && result.terminal == *terminal
        && matches!(
            result.terminal,
            super::SurfaceToolTerminal {
                kind: super::SurfaceToolResultKind::ExternalEffectAmbiguous,
                source: super::ToolTerminalSource::Observed,
                invocation_started: super::ToolInvocationStarted::Yes,
            }
        )
    {
        return true;
    }
    if let [event] = events {
        return matches!(
            (&event.scope, &event.event),
            (
                SurfaceScope::Generation { fence },
                super::SurfaceEvent::Interaction(super::InteractionPatch::Cancelled {
                    reason: super::InteractionCancelReason::CapabilityUnavailable,
                    ..
                }),
            ) if fence == historical_fence
        ) || matches!(
            (&event.scope, &event.event),
            (
                SurfaceScope::Generation { fence },
                super::SurfaceEvent::Tool(
                    super::ToolPatch::CapabilityCallChanged {
                        call: super::SurfaceCapabilityCall {
                            kind:
                                super::SurfaceCapabilityCallKind::ReadTextFile
                                | super::SurfaceCapabilityCallKind::TerminalOutput
                                | super::SurfaceCapabilityCallKind::TerminalWaitForExit,
                            state:
                                super::SurfaceCapabilityCallState::FailedBeforeWrite { .. }
                                | super::SurfaceCapabilityCallState::ObservationUnavailable { .. },
                            ..
                        }
                        | super::SurfaceCapabilityCall {
                            kind: super::SurfaceCapabilityCallKind::WriteTextFile,
                            state:
                                super::SurfaceCapabilityCallState::FailedBeforeWrite { .. }
                                | super::SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                                    effect_kind: super::ExternalEffectKind::FileWrite,
                                    ..
                                },
                            ..
                        }
                        | super::SurfaceCapabilityCall {
                            kind: super::SurfaceCapabilityCallKind::TerminalCreate,
                            state: super::SurfaceCapabilityCallState::FailedBeforeWrite { .. },
                            ..
                        },
                    },
                ),
            ) if fence == historical_fence
        );
    }
    let stops = events
        .iter()
        .filter(|event| recovery_generation_stop_authorized(historical_fence, event))
        .collect::<Vec<_>>();
    if stops.len() != 1 {
        return false;
    }
    let background_fence = match &stops[0].scope {
        SurfaceScope::Background { fence } => Some(fence),
        _ => None,
    };
    let stop_reason = match &stops[0].event {
        super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
            reason, ..
        }) => reason,
        _ => return false,
    };
    let expected_discard_reason = match stop_reason {
        super::GenerationStopReason::Cancelled { .. } => {
            super::AssistantDiscardReason::GenerationCancelled
        }
        super::GenerationStopReason::InterruptedResumable => {
            super::AssistantDiscardReason::GenerationInterrupted
        }
        super::GenerationStopReason::RuntimeRestart
        | super::GenerationStopReason::NotStarted {
            reason: super::NotStartedReason::RuntimeRestart,
        } => super::AssistantDiscardReason::RuntimeRestart,
        super::GenerationStopReason::ProjectionFailure { .. } => {
            super::AssistantDiscardReason::ProjectionRepair
        }
        _ => super::AssistantDiscardReason::ProviderFailed,
    };
    let dispositions = events
        .iter()
        .filter(|event| !recovery_generation_stop_authorized(historical_fence, event))
        .collect::<Vec<_>>();
    let mut operation_dispositions = 0usize;
    dispositions.iter().all(|event| {
        let stream_discard = match (&event.scope, &event.event) {
            (
                SurfaceScope::Generation { fence },
                super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded {
                    reason,
                    ..
                }),
            ) => {
                background_fence.is_none()
                    && fence == historical_fence
                    && *reason == expected_discard_reason
            }
            (
                SurfaceScope::Background { fence },
                super::SurfaceEvent::Assistant(super::AssistantPatch::StreamDiscarded {
                    reason,
                    ..
                }),
            ) => {
                background_fence == Some(fence)
                    && &fence.operation_fence == historical_fence
                    && *reason == expected_discard_reason
            }
            _ => false,
        };
        if stream_discard {
            true
        } else {
            if matches!(
                &event.event,
                super::SurfaceEvent::Operation(
                    super::OperationPatch::Suspended { .. }
                        | super::OperationPatch::SuspensionRebasedAfterUnstartedResume { .. }
                        | super::OperationPatch::FinalizationStarted { .. }
                )
            ) {
                operation_dispositions += 1;
            }
            recovery_event_authorized(historical_fence, background_fence, event)
        }
    }) && operation_dispositions == 1
}

fn recovery_terminal_cleanup_ambiguity_authorized(
    historical_fence: &super::SurfaceOperationFence,
    events: &[super::SurfaceEventEnvelope],
) -> bool {
    if events.len() < 2
        || events.iter().any(|event| {
            !matches!(
                &event.scope,
                SurfaceScope::Generation { fence } if fence == historical_fence
            )
        })
    {
        return false;
    }
    let mut index = 0usize;
    let mut owning_tool_call_id = None;
    while index < events.len()
        && !matches!(
            &events[index].event,
            super::SurfaceEvent::Tool(super::ToolPatch::Completed { .. })
        )
    {
        let Some((consumed, sequence_tool_call_id)) =
            recovery_terminal_cleanup_sequence_authorized(historical_fence, events, index)
        else {
            return false;
        };
        if owning_tool_call_id
            .as_ref()
            .is_some_and(|existing| existing != &sequence_tool_call_id)
        {
            return false;
        }
        owning_tool_call_id.get_or_insert(sequence_tool_call_id);
        index += consumed;
    }
    let Some(owning_tool_call_id) = owning_tool_call_id else {
        return false;
    };
    if index == events.len() {
        return true;
    }
    if events.len() - index != 2 {
        return false;
    }
    ambiguous_tool_completion_authorized(
        historical_fence,
        events,
        index,
        &owning_tool_call_id,
        &mut BTreeSet::new(),
    ) == Some(2)
}

fn recovery_terminal_cleanup_sequence_authorized(
    historical_fence: &super::SurfaceOperationFence,
    events: &[super::SurfaceEventEnvelope],
    index: usize,
) -> Option<(usize, super::SurfaceToolCallId)> {
    let first = events.get(index)?;
    let super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged { call: first_call }) =
        &first.event
    else {
        return None;
    };
    let ambiguous_index = match first_call.state {
        super::SurfaceCapabilityCallState::Prepared => {
            let delivery = events.get(index + 1)?;
            let ambiguous = events.get(index + 2)?;
            let (
                super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                    call: delivery_call,
                }),
                super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                    call: ambiguous_call,
                }),
            ) = (&delivery.event, &ambiguous.event)
            else {
                return None;
            };
            if delivery_call.state != super::SurfaceCapabilityCallState::DeliveryPossible
                || !same_capability_call_identity(first_call, delivery_call)
                || !same_capability_call_identity(first_call, ambiguous_call)
            {
                return None;
            }
            index + 2
        }
        super::SurfaceCapabilityCallState::DeliveryPossible => {
            let ambiguous = events.get(index + 1)?;
            let super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged {
                call: ambiguous_call,
            }) = &ambiguous.event
            else {
                return None;
            };
            if !same_capability_call_identity(first_call, ambiguous_call) {
                return None;
            }
            index + 1
        }
        super::SurfaceCapabilityCallState::ExternalEffectAmbiguous { .. } => index,
        _ => return None,
    };
    let ambiguous_event = events.get(ambiguous_index)?;
    let lease_event = events.get(ambiguous_index + 1)?;
    let (
        super::SurfaceEvent::Tool(super::ToolPatch::CapabilityCallChanged { call: ambiguous }),
        super::SurfaceEvent::Tool(super::ToolPatch::RemoteTerminalLeaseChanged { lease }),
    ) = (&ambiguous_event.event, &lease_event.event)
    else {
        return None;
    };
    let super::SurfaceCapabilityCallState::ExternalEffectAmbiguous { effect_kind, .. } =
        &ambiguous.state
    else {
        return None;
    };
    let super::SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
        terminal_id: Some(terminal_id),
        owner_fence,
    } = &lease.state
    else {
        return None;
    };
    if ambiguous.fence != *historical_fence
        || owner_fence != historical_fence
        || ambiguous.owning_tool_call_id != lease.owning_tool_call_id
        || !matches!(
            (ambiguous.kind, effect_kind),
            (
                super::SurfaceCapabilityCallKind::TerminalKill,
                super::ExternalEffectKind::TerminalKill
            ) | (
                super::SurfaceCapabilityCallKind::TerminalRelease,
                super::ExternalEffectKind::TerminalRelease
            )
        )
        || ambiguous.arguments_digest
            != super::Sha256Digest::new(
                sha2::Sha256::digest(terminal_id.as_str().as_bytes()).into(),
            )
    {
        return None;
    }
    Some((
        ambiguous_index + 2 - index,
        ambiguous.owning_tool_call_id.clone(),
    ))
}

fn recovery_manual_compaction_completion_authorized(
    historical_fence: &super::SurfaceOperationFence,
    events: &[super::SurfaceEventEnvelope],
) -> bool {
    let mut completed = 0usize;
    events.iter().all(|event| {
        if !matches!(
            &event.scope,
            SurfaceScope::Generation { fence } if fence == historical_fence
        ) {
            return false;
        }
        match &event.event {
            super::SurfaceEvent::Item(super::ItemPatch::Removed {
                reason: super::ItemRemovalReason::Compacted,
                ..
            }) => true,
            super::SurfaceEvent::Item(super::ItemPatch::Added { .. }) => true,
            super::SurfaceEvent::Context(super::SurfaceContextSnapshot {
                compaction:
                    super::CompactionState::Completed {
                        operation_id,
                        reason: super::CompactionReason::Manual,
                        ..
                    },
                ..
            }) if operation_id == &historical_fence.operation_id => {
                completed += 1;
                true
            }
            _ => false,
        }
    }) && completed == 1
}

fn recovery_capability_completion_matches_state(
    state: &SurfaceReducerState,
    permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
) -> bool {
    let SurfacePublisherPermit::Recovery {
        historical_fence, ..
    } = permit
    else {
        return true;
    };
    let [completed, item] = batch.events.as_slice() else {
        return true;
    };
    let (
        SurfaceScope::Generation {
            fence: completed_fence,
        },
        super::SurfaceEvent::Tool(super::ToolPatch::Completed { result }),
        SurfaceScope::Generation { fence: item_fence },
        super::SurfaceEvent::Item(super::ItemPatch::Added {
            item:
                super::SurfaceItem::ToolResultMessage {
                    turn_id,
                    tool_call_id,
                    content,
                    terminal,
                    pinned,
                    ..
                },
        }),
    ) = (&completed.scope, &completed.event, &item.scope, &item.event)
    else {
        return true;
    };
    if !matches!(
        result.terminal,
        super::SurfaceToolTerminal {
            kind: super::SurfaceToolResultKind::ExternalEffectAmbiguous,
            source: super::ToolTerminalSource::Observed,
            invocation_started: super::ToolInvocationStarted::Yes,
        }
    ) {
        return true;
    }
    let Some(tool) = state
        .snapshot()
        .tools
        .iter()
        .find(|tool| tool.request.tool_call_id == result.tool_call_id)
    else {
        return false;
    };
    let Some(error) = tool.capability_calls.iter().find_map(|call| {
        if call.fence != *historical_fence {
            return None;
        }
        let super::SurfaceCapabilityCallState::ExternalEffectAmbiguous { error, .. } = &call.state
        else {
            return None;
        };
        Some(error)
    }) else {
        return false;
    };
    let expected_content = super::DisplayText::new(error.as_str());
    let all_capabilities_terminal = tool.capability_calls.iter().all(|call| {
        matches!(
            call.state,
            super::SurfaceCapabilityCallState::Completed { .. }
                | super::SurfaceCapabilityCallState::FailedBeforeWrite { .. }
                | super::SurfaceCapabilityCallState::ObservationUnavailable { .. }
                | super::SurfaceCapabilityCallState::ExternalEffectAmbiguous { .. }
        )
    });
    completed_fence == historical_fence
        && item_fence == historical_fence
        && tool.result.is_none()
        && all_capabilities_terminal
        && result.tool_call_id == tool.request.tool_call_id
        && result.name == tool.request.name
        && result.output.is_none()
        && result.error.as_ref() == Some(&expected_content)
        && result.exit_code.is_none()
        && !result.truncated
        && result.file_change.is_none()
        && turn_id == &tool.request.turn_id
        && tool_call_id == &tool.request.tool_call_id
        && content == &expected_content
        && terminal == &result.terminal
        && !pinned
}

fn recovery_manual_compaction_matches_state(
    state: &SurfaceReducerState,
    permit: &SurfacePublisherPermit,
    batch: &SurfaceCommitBatch,
) -> bool {
    if !matches!(permit, SurfacePublisherPermit::Recovery { .. })
        || !batch.events.as_slice().iter().any(|event| {
            matches!(
                &event.event,
                super::SurfaceEvent::Context(super::SurfaceContextSnapshot {
                    compaction: super::CompactionState::Completed {
                        reason: super::CompactionReason::Manual,
                        ..
                    },
                    ..
                })
            )
        })
    {
        return true;
    }
    manual_compaction_item_rebuild_paired(state.snapshot(), batch)
}

fn recovery_generation_stop_authorized(
    historical_fence: &super::SurfaceOperationFence,
    event: &super::SurfaceEventEnvelope,
) -> bool {
    let exact_scope = matches!(
        (&event.scope, &event.event),
        (
            SurfaceScope::Generation { fence },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                ..
            }),
        ) if fence == historical_fence && patch_fence == historical_fence
    ) || matches!(
        (&event.scope, &event.event),
        (
            SurfaceScope::Background { fence },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                ..
            }),
        ) if &fence.operation_fence == historical_fence && patch_fence == historical_fence
    );
    exact_scope
        && matches!(
            &event.event,
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                reason: super::GenerationStopReason::RuntimeRestart
                    | super::GenerationStopReason::NotStarted {
                        reason: super::NotStartedReason::RuntimeRestart,
                    } | super::GenerationStopReason::ExecutionFailed {
                    class: super::GenerationExecutionFailureClass::ClientCapabilityUnavailable
                        | super::GenerationExecutionFailureClass::RuntimeInvariant
                        | super::GenerationExecutionFailureClass::ExternalEffectAmbiguous
                        | super::GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous,
                    ..
                },
                ..
            })
        )
}

fn recovery_event_authorized(
    historical_fence: &super::SurfaceOperationFence,
    background_fence: Option<&super::SurfaceBackgroundFence>,
    event: &super::SurfaceEventEnvelope,
) -> bool {
    match (&event.scope, &event.event) {
        (
            SurfaceScope::Generation { fence },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                ..
            }),
        ) => {
            background_fence.is_none()
                && fence == historical_fence
                && patch_fence == historical_fence
        }
        (
            SurfaceScope::Background { fence },
            super::SurfaceEvent::Operation(super::OperationPatch::GenerationStopped {
                fence: patch_fence,
                ..
            }),
        ) => {
            background_fence == Some(fence)
                && &fence.operation_fence == historical_fence
                && patch_fence == historical_fence
        }
        (
            SurfaceScope::Operation { operation_id },
            super::SurfaceEvent::Operation(
                super::OperationPatch::Suspended {
                    operation_id: patch_operation,
                    ..
                }
                | super::OperationPatch::SuspensionRebasedAfterUnstartedResume {
                    operation_id: patch_operation,
                    ..
                }
                | super::OperationPatch::FinalizationStarted {
                    operation_id: patch_operation,
                    ..
                },
            ),
        ) => {
            background_fence.is_none()
                && operation_id == &historical_fence.operation_id
                && patch_operation == &historical_fence.operation_id
        }
        (
            SurfaceScope::Generation { fence },
            super::SurfaceEvent::Interaction(super::InteractionPatch::Cancelled {
                reason: super::InteractionCancelReason::CapabilityUnavailable,
                ..
            }),
        ) => background_fence.is_none() && fence == historical_fence,
        (
            SurfaceScope::Background { fence },
            super::SurfaceEvent::Operation(
                super::OperationPatch::Suspended {
                    operation_id: patch_operation,
                    ..
                }
                | super::OperationPatch::SuspensionRebasedAfterUnstartedResume {
                    operation_id: patch_operation,
                    ..
                }
                | super::OperationPatch::FinalizationStarted {
                    operation_id: patch_operation,
                    ..
                },
            ),
        ) => {
            background_fence == Some(fence)
                && &fence.operation_fence == historical_fence
                && patch_operation == &historical_fence.operation_id
        }
        _ => false,
    }
}

fn next_permit_id() -> super::SurfacePublisherPermitId {
    let first = uuid::Uuid::now_v7();
    let second = uuid::Uuid::now_v7();
    let mut bytes = [0; 32];
    bytes[..16].copy_from_slice(first.as_bytes());
    bytes[16..].copy_from_slice(second.as_bytes());
    super::SurfacePublisherPermitId::new(bytes)
}

fn recovered_operation_usage(
    snapshot: &super::SurfaceSnapshot,
    operation_id: &super::SurfaceOperationId,
) -> super::UsageTotals {
    snapshot
        .usage
        .active_operation
        .as_ref()
        .filter(|(active, _)| active == operation_id)
        .map(|(_, usage)| usage.clone())
        .unwrap_or_else(zero_usage)
}

fn terminal_from_terminalization(cause: super::TerminalizationCause) -> super::OperationTerminal {
    match cause {
        super::TerminalizationCause::UserCancel => super::OperationTerminal::Cancelled {
            reason: super::CancelReason::User,
        },
        super::TerminalizationCause::GoalPause => super::OperationTerminal::Cancelled {
            reason: super::CancelReason::GoalPause,
        },
        super::TerminalizationCause::HostShutdown => super::OperationTerminal::Shutdown {
            reason: super::SurfaceShutdownReason::HostShutdown,
        },
        super::TerminalizationCause::ThreadClose => super::OperationTerminal::Shutdown {
            reason: super::SurfaceShutdownReason::ThreadClose,
        },
    }
}

fn terminal_failure_class(class: super::GenerationExecutionFailureClass) -> super::FailureClass {
    match class {
        super::GenerationExecutionFailureClass::Provider => super::FailureClass::Provider,
        super::GenerationExecutionFailureClass::Tool => super::FailureClass::Tool,
        super::GenerationExecutionFailureClass::Hook => super::FailureClass::Hook,
        super::GenerationExecutionFailureClass::Workflow => super::FailureClass::Workflow,
        super::GenerationExecutionFailureClass::InputResolution => {
            super::FailureClass::InputResolution
        }
        super::GenerationExecutionFailureClass::ClientCapabilityUnavailable => {
            super::FailureClass::ClientCapabilityUnavailable
        }
        super::GenerationExecutionFailureClass::LegacyApprovalRequired => {
            super::FailureClass::LegacyApprovalRequired
        }
        super::GenerationExecutionFailureClass::RuntimeInvariant => {
            super::FailureClass::RuntimeInvariant
        }
        super::GenerationExecutionFailureClass::ExternalEffectAmbiguous => {
            super::FailureClass::ExternalEffectAmbiguous
        }
        super::GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous => {
            super::FailureClass::RemoteResourceCleanupAmbiguous
        }
    }
}

fn terminal_from_generation_stop(
    operation: &super::OperationRecord,
    reason: &super::GenerationStopReason,
    usage: &super::UsageTotals,
) -> Result<super::OperationTerminal, SurfaceCommitError> {
    let last_generation = || {
        operation
            .generations
            .last()
            .map(|generation| generation.fence.generation_id)
            .ok_or(SurfaceCommitError::CursorRangeAlreadyConsumed)
    };
    Ok(match reason {
        super::GenerationStopReason::Completed { status } => match status {
            super::GenerationCompletionStatus::Success => super::OperationTerminal::Succeeded {
                usage: usage.clone(),
            },
            super::GenerationCompletionStatus::VerificationFailed { message } => {
                super::OperationTerminal::Failed {
                    class: super::FailureClass::Verification,
                    message: message.clone(),
                }
            }
            super::GenerationCompletionStatus::BudgetExhausted { budget } => {
                super::OperationTerminal::BudgetExhausted {
                    budget: budget.clone(),
                }
            }
        },
        super::GenerationStopReason::Cancelled { cause } => terminal_from_terminalization(*cause),
        super::GenerationStopReason::InterruptedResumable
        | super::GenerationStopReason::ProviderSuspended
        | super::GenerationStopReason::RuntimeRestart => {
            super::OperationTerminal::AbortedByRuntimeRestart {
                last_generation: last_generation()?,
            }
        }
        super::GenerationStopReason::ProjectionFailure { message } => {
            super::OperationTerminal::Failed {
                class: super::FailureClass::Persistence,
                message: message.clone(),
            }
        }
        super::GenerationStopReason::ExecutionFailed { class, message } => {
            super::OperationTerminal::Failed {
                class: terminal_failure_class(*class),
                message: message.clone(),
            }
        }
        super::GenerationStopReason::Panicked { message } => super::OperationTerminal::Panicked {
            message: message.clone(),
        },
        super::GenerationStopReason::NotStarted { reason } => match reason {
            super::NotStartedReason::ReservationExpired => super::OperationTerminal::NotAdmitted {
                reason: super::NotAdmittedReason::ReservationExpired,
            },
            super::NotStartedReason::Cancelled { cause } => terminal_from_terminalization(*cause),
            super::NotStartedReason::Interrupted | super::NotStartedReason::RuntimeRestart => {
                super::OperationTerminal::AbortedByRuntimeRestart {
                    last_generation: last_generation()?,
                }
            }
            super::NotStartedReason::StartCommitFailure { message } => {
                super::OperationTerminal::Failed {
                    class: super::FailureClass::Persistence,
                    message: message.clone(),
                }
            }
            super::NotStartedReason::MissingLiveInputCapsule => super::OperationTerminal::Failed {
                class: super::FailureClass::RuntimeInvariant,
                message: super::SafeDiagnosticText::try_new(
                    "non-replayable operation input capsule is unavailable before generation start",
                )
                .expect("static diagnostic is valid"),
            },
            super::NotStartedReason::AdmissionRejected { reason } => {
                super::OperationTerminal::NotAdmitted {
                    reason: match reason {
                        super::AdmissionRejectionReason::ConfigurationConflict => {
                            super::NotAdmittedReason::ConfigurationConflict
                        }
                        super::AdmissionRejectionReason::PolicyConflict => {
                            super::NotAdmittedReason::PolicyConflict
                        }
                    },
                }
            }
            super::NotStartedReason::Shutdown { reason } => {
                super::OperationTerminal::Shutdown { reason: *reason }
            }
        },
    })
}

fn terminal_from_finalization(
    operation: &super::OperationRecord,
    finalization: &super::OperationFinalizationRecord,
    usage: &super::UsageTotals,
) -> Result<super::OperationTerminal, SurfaceCommitError> {
    Ok(match &finalization.selected_cause {
        super::OperationFinalizationCause::Terminalization(cause) => {
            terminal_from_terminalization(*cause)
        }
        super::OperationFinalizationCause::GenerationStop(reason) => {
            terminal_from_generation_stop(operation, reason, usage)?
        }
        super::OperationFinalizationCause::Reservation(reason) => {
            super::OperationTerminal::NotAdmitted {
                reason: match reason {
                    super::ReservationFinalizerReason::ReservationExpired => {
                        super::NotAdmittedReason::ReservationExpired
                    }
                    super::ReservationFinalizerReason::AdmissionRejected { reason } => match reason
                    {
                        super::AdmissionRejectionReason::ConfigurationConflict => {
                            super::NotAdmittedReason::ConfigurationConflict
                        }
                        super::AdmissionRejectionReason::PolicyConflict => {
                            super::NotAdmittedReason::PolicyConflict
                        }
                    },
                    super::ReservationFinalizerReason::CancelledBeforeAdmission => {
                        super::NotAdmittedReason::CancelledBeforeAdmission
                    }
                    super::ReservationFinalizerReason::RuntimeRestart => {
                        super::NotAdmittedReason::RuntimeRestart
                    }
                    super::ReservationFinalizerReason::HostShutdown => {
                        super::NotAdmittedReason::HostShutdown
                    }
                    super::ReservationFinalizerReason::ThreadClose => {
                        super::NotAdmittedReason::ThreadClose
                    }
                },
            }
        }
        super::OperationFinalizationCause::OperationJoinSettlement(source) => {
            super::OperationTerminal::JoinFailed {
                message: source.message.clone(),
            }
        }
        super::OperationFinalizationCause::Suspended(cause) => match cause {
            super::SuspendedFinalizationCause::Terminalization(cause) => {
                terminal_from_terminalization(*cause)
            }
            super::SuspendedFinalizationCause::ResumeStartCommitFailure { message } => {
                super::OperationTerminal::Failed {
                    class: super::FailureClass::Persistence,
                    message: message.clone(),
                }
            }
            super::SuspendedFinalizationCause::RecoveryAbortNonReplayable { last_generation } => {
                super::OperationTerminal::AbortedByRuntimeRestart {
                    last_generation: *last_generation,
                }
            }
        },
    })
}

fn prepared_identity(batch: &SurfaceCommitBatch) -> PreparedSurfaceCommit {
    PreparedSurfaceCommit {
        commit_id: commit_id(&batch.commit_class).clone(),
        event_count: batch.event_count,
        batch_digest: batch.batch_digest.clone(),
        cursor_before: batch.cursor_before.clone(),
        cursor_after: batch.cursor_after.clone(),
    }
}

fn commit_id(class: &CommitClass) -> &SurfaceCommitId {
    match class {
        CommitClass::Recorded { commit_id, .. } | CommitClass::Ephemeral { commit_id, .. } => {
            commit_id
        }
    }
}

fn zero_usage() -> super::UsageTotals {
    super::UsageTotals {
        input_tokens: 0,
        output_tokens: 0,
        cache_tokens: 0,
        estimated_cost_usd_micros: 0,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryReplayability {
    NotApplicable,
    Replayable,
    NonReplayableCurrent,
    NonReplayableNotCurrent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryMaterialization {
    SameProcessProjectionReset,
    ColdOwnerTakeover,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryDegradedCause {
    MissingFinalization,
    TerminalProjectionPending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoverySourcePhase {
    Requested,
    Reserved,
    StartedOrTransferred {
        exact_terminal_interaction_unavailable: bool,
    },
    Suspended,
    ResumeStartingReserved,
    Finalizing,
    FinalizingDegraded {
        cause: RecoveryDegradedCause,
    },
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryAction {
    FinalizeRequested,
    StopAndSuspend,
    StopAndFinalizeRecoveryAbort,
    StopAndFinalizeClientCapabilityUnavailable,
    StopAndFinalizeRuntimeRestart,
    ExposeRecoveryRequired,
    FinalizeRecoveryAbort,
    StopAndRebaseSuspension,
    ReconcileOriginalFinalizer,
    ExposeRetryFinalization,
    ExposeRetryProjection,
    NoOp,
}

pub fn decide_post_materialization_recovery(
    phase: RecoverySourcePhase,
    replayability: RecoveryReplayability,
    materialization: RecoveryMaterialization,
) -> RecoveryAction {
    use RecoveryAction::*;
    use RecoveryReplayability::*;
    use RecoverySourcePhase::*;

    let current_live_capsule = matches!(
        (replayability, materialization),
        (Replayable, _)
            | (
                NonReplayableCurrent,
                RecoveryMaterialization::SameProcessProjectionReset
            )
    );
    match phase {
        Requested => FinalizeRequested,
        Reserved if current_live_capsule => StopAndSuspend,
        Reserved => StopAndFinalizeRecoveryAbort,
        StartedOrTransferred {
            exact_terminal_interaction_unavailable: true,
        } => StopAndFinalizeClientCapabilityUnavailable,
        StartedOrTransferred { .. } => StopAndFinalizeRuntimeRestart,
        Suspended if current_live_capsule => ExposeRecoveryRequired,
        Suspended => FinalizeRecoveryAbort,
        ResumeStartingReserved if current_live_capsule => StopAndRebaseSuspension,
        ResumeStartingReserved => StopAndFinalizeRecoveryAbort,
        Finalizing => ReconcileOriginalFinalizer,
        FinalizingDegraded {
            cause: RecoveryDegradedCause::MissingFinalization,
        } => ExposeRetryFinalization,
        FinalizingDegraded {
            cause: RecoveryDegradedCause::TerminalProjectionPending,
        } => ExposeRetryProjection,
        Terminal => NoOp,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownPlanError {
    ImmutableConflict,
    MissingDurableBarrier,
    IncompleteBarrier,
    OutputScopeMismatch,
}

#[derive(Clone, Debug, Default)]
pub struct ImmutableShutdownLedger {
    record: Option<super::ShutdownBarrierRecord>,
    durable_plan: bool,
}

impl ImmutableShutdownLedger {
    pub(crate) fn from_durable_record(
        record: super::ShutdownBarrierRecord,
    ) -> Result<Self, ShutdownPlanError> {
        validate_shutdown_plan(&record.plan)?;
        validate_shutdown_settlements(&record.plan, &record.settled)?;
        if let super::ShutdownBarrierState::Closed { retained_output } = &record.state {
            validate_closed_shutdown_record(&record.plan, &record.settled, retained_output)?;
        }
        Ok(Self {
            record: Some(record),
            durable_plan: true,
        })
    }

    pub(crate) fn durable_record(&self) -> Option<&super::ShutdownBarrierRecord> {
        self.record.as_ref()
    }

    pub(crate) fn mark_plan_durable(
        &mut self,
        expected: &super::ShutdownBarrierPlan,
    ) -> Result<(), ShutdownPlanError> {
        if self.record.as_ref().map(|record| &record.plan) != Some(expected) {
            return Err(ShutdownPlanError::ImmutableConflict);
        }
        self.durable_plan = true;
        Ok(())
    }

    pub fn record(
        &mut self,
        plan: super::ShutdownBarrierPlan,
    ) -> Result<&super::ShutdownBarrierPlan, ShutdownPlanError> {
        if self.record.is_some() {
            return if self.record.as_ref().map(|record| &record.plan) == Some(&plan) {
                Ok(&self.record.as_ref().expect("shutdown record exists").plan)
            } else {
                Err(ShutdownPlanError::ImmutableConflict)
            };
        }
        self.record = Some(super::ShutdownBarrierRecord {
            plan,
            settled: Vec::new(),
            state: super::ShutdownBarrierState::Closing,
        });
        Ok(&self
            .record
            .as_ref()
            .expect("shutdown record was inserted")
            .plan)
    }

    pub fn plan(&self) -> Option<&super::ShutdownBarrierPlan> {
        self.record.as_ref().map(|record| &record.plan)
    }

    pub fn signal_authorized(&self) -> bool {
        self.record.is_some() && self.durable_plan
    }

    pub fn settle(
        &mut self,
        acknowledgement: super::MutationCommitAck,
    ) -> Result<(), ShutdownPlanError> {
        if !self.durable_plan {
            return Err(ShutdownPlanError::MissingDurableBarrier);
        }
        let record = self
            .record
            .as_mut()
            .ok_or(ShutdownPlanError::MissingDurableBarrier)?;
        if matches!(record.state, super::ShutdownBarrierState::Closed { .. }) {
            return Err(ShutdownPlanError::ImmutableConflict);
        }
        if record.settled.contains(&acknowledgement) {
            return Ok(());
        }
        if !shutdown_ack_matches_plan(&record.plan, &acknowledgement)
            || record
                .settled
                .iter()
                .any(|existing| shutdown_acks_target_same_requirement(existing, &acknowledgement))
        {
            return Err(ShutdownPlanError::OutputScopeMismatch);
        }
        record.settled.push(acknowledgement);
        Ok(())
    }

    pub fn close(
        &mut self,
        output: super::RetainedShutdownOutput,
    ) -> Result<super::RetainedShutdownOutput, ShutdownPlanError> {
        if !self.durable_plan {
            return Err(ShutdownPlanError::MissingDurableBarrier);
        }
        let record = self
            .record
            .as_mut()
            .ok_or(ShutdownPlanError::MissingDurableBarrier)?;
        let matching_scope = match (&record.plan, &output) {
            (
                super::ShutdownBarrierPlan::CloseThread { thread, .. },
                super::RetainedShutdownOutput::CloseThread { output },
            ) => shutdown_thread_output_matches(thread, output),
            (
                super::ShutdownBarrierPlan::ShutdownHost {
                    host_incarnation,
                    threads,
                    ..
                },
                super::RetainedShutdownOutput::ShutdownHost { output },
            ) => {
                &output.host_incarnation == host_incarnation
                    && threads.len() == output.closed_threads.len()
                    && threads.iter().all(|plan| {
                        output
                            .closed_threads
                            .iter()
                            .any(|closed| shutdown_thread_output_matches(plan, closed))
                    })
            }
            _ => false,
        };
        if !matching_scope {
            return Err(ShutdownPlanError::OutputScopeMismatch);
        }
        let existing_output = match &record.state {
            super::ShutdownBarrierState::Closed { retained_output } => Some(retained_output),
            super::ShutdownBarrierState::Closing => None,
        };
        if let Some(existing_output) = existing_output {
            if existing_output != &output {
                return Err(ShutdownPlanError::ImmutableConflict);
            }
            return Ok(existing_output.clone());
        }
        validate_closed_shutdown_record(&record.plan, &record.settled, &output)?;
        record.state = super::ShutdownBarrierState::Closed {
            retained_output: output,
        };
        match &record.state {
            super::ShutdownBarrierState::Closed { retained_output } => Ok(retained_output.clone()),
            super::ShutdownBarrierState::Closing => unreachable!(),
        }
    }

    pub fn retained_output(&self) -> Option<&super::RetainedShutdownOutput> {
        match &self.record.as_ref()?.state {
            super::ShutdownBarrierState::Closed { retained_output } => Some(retained_output),
            super::ShutdownBarrierState::Closing => None,
        }
    }
}

fn validate_shutdown_plan(plan: &super::ShutdownBarrierPlan) -> Result<(), ShutdownPlanError> {
    let mut thread_ids = std::collections::BTreeSet::new();
    let validate_thread = |thread: &super::ShutdownThreadPlan| {
        let (thread_id, owner_epoch, operations, session_closed, catalog_closed) = match thread {
            super::ShutdownThreadPlan::Recorded {
                thread_id,
                owner_epoch,
                operations,
                session_closed,
                catalog_closed,
            } => (
                thread_id,
                owner_epoch,
                operations,
                session_closed,
                Some(catalog_closed),
            ),
            super::ShutdownThreadPlan::Ephemeral {
                thread_id,
                owner_epoch,
                operations,
                session_closed,
                ..
            } => (thread_id, owner_epoch, operations, session_closed, None),
        };
        if session_closed.thread_id != *thread_id
            || session_closed.family != SurfaceFactFamily::Session
        {
            return false;
        }
        if let Some(catalog) = catalog_closed {
            if !matches!(
                &catalog.identity,
                super::HostReceiptRequirementIdentity::SessionCatalog {
                    thread_id: Some(expected), ..
                } if expected == thread_id
            ) {
                return false;
            }
        }
        operations.iter().all(|operation| {
            let (operation_id, finalize_intent_id, terminal_commit_id, requirement) =
                match operation {
                    super::ShutdownOperationPlan::ExistingTerminal {
                        operation_id,
                        finalize_intent_id,
                        terminal_commit_id,
                        requirement,
                    }
                    | super::ShutdownOperationPlan::PlannedFinalization {
                        operation_id,
                        finalize_intent_id,
                        terminal_commit_id,
                        requirement,
                        ..
                    } => (
                        operation_id,
                        finalize_intent_id,
                        terminal_commit_id,
                        requirement,
                    ),
                };
            let _ = finalize_intent_id;
            requirement.thread_id == *thread_id
                && requirement.thread_owner_epoch == *owner_epoch
                && requirement.operation_id == *operation_id
                && requirement.terminal_commit_id == *terminal_commit_id
        })
    };

    match plan {
        super::ShutdownBarrierPlan::CloseThread { thread, .. } => {
            if validate_thread(thread) {
                Ok(())
            } else {
                Err(ShutdownPlanError::OutputScopeMismatch)
            }
        }
        super::ShutdownBarrierPlan::ShutdownHost {
            host_incarnation,
            threads,
            final_host_lifecycle,
            ..
        } => {
            let lifecycle_matches = final_host_lifecycle.host_incarnation == *host_incarnation
                && matches!(
                    &final_host_lifecycle.identity,
                    super::HostReceiptRequirementIdentity::HostLifecycle {
                        host_incarnation: identity_host, ..
                    } if identity_host == host_incarnation
                );
            if lifecycle_matches
                && threads.iter().all(|thread| {
                    let thread_id = shutdown_thread_id(thread);
                    thread_ids.insert(thread_id.clone()) && validate_thread(thread)
                })
            {
                Ok(())
            } else {
                Err(ShutdownPlanError::OutputScopeMismatch)
            }
        }
    }
}

fn validate_shutdown_settlements(
    plan: &super::ShutdownBarrierPlan,
    settled: &[super::MutationCommitAck],
) -> Result<(), ShutdownPlanError> {
    if settled
        .iter()
        .any(|ack| !shutdown_ack_matches_plan(plan, ack))
        || settled.iter().enumerate().any(|(index, ack)| {
            settled[..index]
                .iter()
                .any(|prior| shutdown_acks_target_same_requirement(prior, ack))
        })
    {
        Err(ShutdownPlanError::OutputScopeMismatch)
    } else {
        Ok(())
    }
}

fn validate_closed_shutdown_record(
    plan: &super::ShutdownBarrierPlan,
    settled: &[super::MutationCommitAck],
    output: &super::RetainedShutdownOutput,
) -> Result<(), ShutdownPlanError> {
    let outputs_match =
        match (plan, output) {
            (
                super::ShutdownBarrierPlan::CloseThread { thread, .. },
                super::RetainedShutdownOutput::CloseThread { output },
            ) => shutdown_thread_is_fully_settled(thread, output, settled),
            (
                super::ShutdownBarrierPlan::ShutdownHost {
                    host_incarnation,
                    threads,
                    final_host_lifecycle,
                    ..
                },
                super::RetainedShutdownOutput::ShutdownHost { output },
            ) => &output.host_incarnation == host_incarnation
                && threads.len() == output.closed_threads.len()
                && threads.iter().all(|thread| {
                    output
                        .closed_threads
                        .iter()
                        .any(|closed| shutdown_thread_is_fully_settled(thread, closed, settled))
                }) && settled.iter().any(|ack| {
                host_ack_matches_requirement(final_host_lifecycle, ack)
                    && matches!(
                        ack,
                        super::MutationCommitAck::HostCommitAck {
                            identity: super::HostReceiptIdentityPair::HostLifecycle { receipt, .. },
                            ..
                        } if receipt == &output.host_receipt
                    )
            }),
            _ => false,
        };
    if outputs_match {
        Ok(())
    } else {
        Err(ShutdownPlanError::IncompleteBarrier)
    }
}

fn shutdown_thread_is_fully_settled(
    plan: &super::ShutdownThreadPlan,
    output: &super::ClosedThreadReceipt,
    settled: &[super::MutationCommitAck],
) -> bool {
    if !shutdown_thread_output_matches(plan, output) {
        return false;
    }
    let (operations, session_closed, catalog_closed, closed_cursor, output_terminals) = match (
        plan, output,
    ) {
        (
            super::ShutdownThreadPlan::Recorded {
                operations,
                session_closed,
                catalog_closed,
                ..
            },
            super::ClosedThreadReceipt::Recorded {
                operation_terminals,
                closed_cursor,
                catalog_receipt,
                ..
            },
        ) => {
            if !settled.iter().any(|ack| {
                    host_ack_matches_requirement(catalog_closed, ack)
                        && matches!(
                            ack,
                            super::MutationCommitAck::HostCommitAck {
                                identity: super::HostReceiptIdentityPair::SessionCatalog { receipt, .. },
                                ..
                            } if receipt == catalog_receipt
                        )
                }) {
                    return false;
                }
            (
                operations,
                session_closed,
                Some(catalog_closed),
                closed_cursor,
                operation_terminals,
            )
        }
        (
            super::ShutdownThreadPlan::Ephemeral {
                operations,
                session_closed,
                ..
            },
            super::ClosedThreadReceipt::Ephemeral {
                operation_terminals,
                closed_cursor,
                ..
            },
        ) => (
            operations,
            session_closed,
            None,
            closed_cursor,
            operation_terminals,
        ),
        _ => return false,
    };
    let _ = catalog_closed;
    let session_matches = settled.iter().any(|ack| {
        thread_ack_matches_requirement(plan, session_closed, ack)
            && matches!(ack, super::MutationCommitAck::ThreadLocalCursor { cursor, .. } if cursor == closed_cursor)
    });
    let operations_match = operations.len() == output_terminals.len()
        && operations.iter().all(|operation| {
            let requirement = shutdown_operation_requirement(operation);
            settled.iter().any(|ack| {
                operation_ack_matches_requirement(requirement, ack)
                    && matches!(
                        ack,
                        super::MutationCommitAck::OperationTerminalAck { value, .. }
                            if output_terminals.contains(value)
                    )
            })
        });
    session_matches && operations_match
}

fn shutdown_thread_id(thread: &super::ShutdownThreadPlan) -> &super::SurfaceThreadId {
    match thread {
        super::ShutdownThreadPlan::Recorded { thread_id, .. }
        | super::ShutdownThreadPlan::Ephemeral { thread_id, .. } => thread_id,
    }
}

fn shutdown_operation_requirement(
    operation: &super::ShutdownOperationPlan,
) -> &super::OperationTerminalAckRequirement {
    match operation {
        super::ShutdownOperationPlan::ExistingTerminal { requirement, .. }
        | super::ShutdownOperationPlan::PlannedFinalization { requirement, .. } => requirement,
    }
}

fn shutdown_ack_matches_plan(
    plan: &super::ShutdownBarrierPlan,
    acknowledgement: &super::MutationCommitAck,
) -> bool {
    let thread_matches = |thread: &super::ShutdownThreadPlan| {
        let (operations, session_closed, catalog_closed) = match thread {
            super::ShutdownThreadPlan::Recorded {
                operations,
                session_closed,
                catalog_closed,
                ..
            } => (operations, session_closed, Some(catalog_closed)),
            super::ShutdownThreadPlan::Ephemeral {
                operations,
                session_closed,
                ..
            } => (operations, session_closed, None),
        };
        thread_ack_matches_requirement(thread, session_closed, acknowledgement)
            || catalog_closed.is_some_and(|requirement| {
                host_ack_matches_requirement(requirement, acknowledgement)
            })
            || operations.iter().any(|operation| {
                operation_ack_matches_requirement(
                    shutdown_operation_requirement(operation),
                    acknowledgement,
                )
            })
    };
    match plan {
        super::ShutdownBarrierPlan::CloseThread { thread, .. } => thread_matches(thread),
        super::ShutdownBarrierPlan::ShutdownHost {
            threads,
            final_host_lifecycle,
            ..
        } => {
            threads.iter().any(thread_matches)
                || host_ack_matches_requirement(final_host_lifecycle, acknowledgement)
        }
    }
}

fn thread_ack_matches_requirement(
    thread: &super::ShutdownThreadPlan,
    requirement: &super::ThreadCursorAckRequirement,
    acknowledgement: &super::MutationCommitAck,
) -> bool {
    let owner_epoch = match thread {
        super::ShutdownThreadPlan::Recorded { owner_epoch, .. }
        | super::ShutdownThreadPlan::Ephemeral { owner_epoch, .. } => owner_epoch,
    };
    matches!(
        acknowledgement,
        super::MutationCommitAck::ThreadLocalCursor {
            cursor,
            family,
            event_id,
            commit_class:
                CommitClass::Recorded {
                    thread_owner_epoch,
                    commit_id,
                    ..
                },
        } if cursor.thread_id == requirement.thread_id
            && family == &requirement.family
            && event_id == &requirement.event_id
            && commit_id == &requirement.commit_id
            && thread_owner_epoch == owner_epoch
    )
}

fn operation_ack_matches_requirement(
    requirement: &super::OperationTerminalAckRequirement,
    acknowledgement: &super::MutationCommitAck,
) -> bool {
    matches!(
        acknowledgement,
        super::MutationCommitAck::OperationTerminalAck {
            thread_id,
            thread_owner_epoch,
            operation_id,
            value,
        } if thread_id == &requirement.thread_id
            && thread_owner_epoch == &requirement.thread_owner_epoch
            && operation_id == &requirement.operation_id
            && value.operation_id == requirement.operation_id
            && value.cursor.thread_id == requirement.thread_id
            && matches!(
                &value.commit_class,
                CommitClass::Recorded {
                    thread_owner_epoch,
                    commit_id,
                    ..
                } if *thread_owner_epoch == requirement.thread_owner_epoch
                    && commit_id == &requirement.terminal_commit_id
            )
    )
}

fn host_ack_matches_requirement(
    requirement: &super::HostReceiptAckRequirement,
    acknowledgement: &super::MutationCommitAck,
) -> bool {
    let super::MutationCommitAck::HostCommitAck {
        host_incarnation,
        identity,
        commit_id,
        receipt_digest,
    } = acknowledgement
    else {
        return false;
    };
    if host_incarnation != &requirement.host_incarnation
        || commit_id != &requirement.commit_id
        || receipt_digest != &requirement.receipt_digest
    {
        return false;
    }
    match (&requirement.identity, identity) {
        (
            super::HostReceiptRequirementIdentity::SessionCatalog {
                thread_id,
                revision,
            },
            super::HostReceiptIdentityPair::SessionCatalog {
                thread_id: ack_thread,
                revision: ack_revision,
                receipt,
            },
        ) => {
            thread_id == ack_thread
                && revision == ack_revision
                && receipt.thread_id == *thread_id
                && receipt.catalog_revision == *revision
                && receipt.action == super::SurfaceSessionCatalogAction::Closed
        }
        (
            super::HostReceiptRequirementIdentity::HostLifecycle {
                host_incarnation,
                revision,
            },
            super::HostReceiptIdentityPair::HostLifecycle {
                host_incarnation: ack_host,
                revision: ack_revision,
                receipt,
            },
        ) => {
            host_incarnation == ack_host
                && revision == ack_revision
                && receipt.host_incarnation == *host_incarnation
                && receipt.lifecycle_revision == *revision
                && receipt.shutdown_commit_id == requirement.commit_id
                && receipt.stage == super::SurfaceHostShutdownStage::Last
        }
        _ => false,
    }
}

fn shutdown_acks_target_same_requirement(
    first: &super::MutationCommitAck,
    second: &super::MutationCommitAck,
) -> bool {
    match (first, second) {
        (
            super::MutationCommitAck::ThreadLocalCursor {
                cursor: first_cursor,
                family: first_family,
                ..
            },
            super::MutationCommitAck::ThreadLocalCursor {
                cursor: second_cursor,
                family: second_family,
                ..
            },
        ) => first_cursor.thread_id == second_cursor.thread_id && first_family == second_family,
        (
            super::MutationCommitAck::OperationTerminalAck {
                operation_id: first_operation,
                ..
            },
            super::MutationCommitAck::OperationTerminalAck {
                operation_id: second_operation,
                ..
            },
        ) => first_operation == second_operation,
        (
            super::MutationCommitAck::HostCommitAck {
                host_incarnation: first_host,
                commit_id: first_commit,
                ..
            },
            super::MutationCommitAck::HostCommitAck {
                host_incarnation: second_host,
                commit_id: second_commit,
                ..
            },
        ) => first_host == second_host && first_commit == second_commit,
        _ => false,
    }
}

fn shutdown_thread_output_matches(
    plan: &super::ShutdownThreadPlan,
    output: &super::ClosedThreadReceipt,
) -> bool {
    match (plan, output) {
        (
            super::ShutdownThreadPlan::Recorded {
                thread_id,
                operations,
                ..
            },
            super::ClosedThreadReceipt::Recorded {
                thread_id: output_thread_id,
                operation_terminals,
                ..
            },
        ) => {
            thread_id == output_thread_id
                && shutdown_operation_outputs_match(operations, operation_terminals)
        }
        (
            super::ShutdownThreadPlan::Ephemeral {
                thread_id,
                persistence,
                operations,
                ..
            },
            super::ClosedThreadReceipt::Ephemeral {
                thread_id: output_thread_id,
                persistence: output_persistence,
                operation_terminals,
                ..
            },
        ) => {
            thread_id == output_thread_id
                && persistence == output_persistence
                && shutdown_operation_outputs_match(operations, operation_terminals)
        }
        _ => false,
    }
}

fn shutdown_operation_outputs_match(
    plans: &[super::ShutdownOperationPlan],
    outputs: &[super::OperationTerminalAtCursor],
) -> bool {
    plans.len() == outputs.len()
        && plans.iter().all(|plan| {
            let requirement = match plan {
                super::ShutdownOperationPlan::ExistingTerminal { requirement, .. }
                | super::ShutdownOperationPlan::PlannedFinalization { requirement, .. } => {
                    requirement
                }
            };
            outputs.iter().any(|output| {
                output.operation_id == requirement.operation_id
                    && matches!(
                        &output.commit_class,
                        CommitClass::Recorded {
                            thread_owner_epoch,
                            commit_id,
                            ..
                        } if *thread_owner_epoch == requirement.thread_owner_epoch
                            && commit_id == &requirement.terminal_commit_id
                    )
                    && output.cursor.thread_id == requirement.thread_id
            })
        })
}

pub fn select_shutdown_cause(
    existing: Option<super::OperationFinalizationCause>,
    requested: super::ShutdownRequestCause,
) -> super::ShutdownSelectedCause {
    match existing {
        Some(cause) => super::ShutdownSelectedCause::ExistingWinning { cause },
        None => super::ShutdownSelectedCause::Requested { cause: requested },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_surface::DurableBatchReceipt;
    use crate::runtime_surface::reducer::tests::{
        digest, reducer_snapshot, started_operation, thread_id, uuid_v7_bytes,
    };
    use crate::tasks::{LegacyActiveTaskAdoptionRecord, TaskRegistry};

    #[derive(Default)]
    struct TestLedger {
        writes: usize,
        receipt: Option<SurfaceBatchReceipt>,
    }

    impl SurfaceCommitLedger for TestLedger {
        fn append_complete_batch(
            &mut self,
            batch: &SurfaceCommitBatch,
        ) -> Result<SurfaceBatchReceipt, SurfaceLedgerError> {
            self.writes += 1;
            let CommitClass::Recorded {
                commit_id,
                durable_revision,
                ..
            } = &batch.commit_class
            else {
                unreachable!();
            };
            let receipt = DurableBatchReceipt {
                commit_id: commit_id.clone(),
                durable_revision: *durable_revision,
                event_count: batch.event_count,
                batch_digest: batch.batch_digest.clone(),
                cursor_after: batch.cursor_after.clone(),
            };
            let receipt = SurfaceBatchReceipt::Recorded(receipt);
            self.receipt = Some(receipt.clone());
            Ok(receipt)
        }

        fn checkpoint(&mut self, _receipt: &SurfaceBatchReceipt) -> Result<(), SurfaceLedgerError> {
            self.writes += 1;
            Ok(())
        }

        fn probe_commit(
            &self,
            _id: &SurfaceCommitId,
            _digest: &super::super::Sha256Digest,
        ) -> CommitProbe {
            self.receipt
                .clone()
                .map(CommitProbe::Present)
                .unwrap_or(CommitProbe::Absent)
        }
    }

    struct TestClock;

    impl super::super::InjectedRuntimeClock for TestClock {
        fn clock_id(&self) -> super::super::HostMonotonicClockId {
            super::super::HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(90)).unwrap()
        }

        fn monotonic_tick(&self) -> u64 {
            1
        }

        fn wall_clock_ms(&self) -> i64 {
            1
        }
    }

    fn next_test_commit_identity(
        state: &SurfaceReducerState,
    ) -> (super::super::DurableRevision, SurfaceCommitId) {
        let super::super::CursorSourceRevision::Recorded { durable_revision } =
            state.snapshot().cursor.source_revision
        else {
            panic!("commit test batch requires a recorded cursor");
        };
        let next_revision =
            super::super::DurableRevision::try_new(durable_revision.get().checked_add(1).unwrap())
                .unwrap();
        let seed = 90 + u8::try_from(durable_revision.get()).unwrap();
        (
            next_revision,
            SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
        )
    }

    fn test_batch(state: &SurfaceReducerState) -> SurfaceCommitBatch {
        let (durable_revision, commit_id) = next_test_commit_identity(state);
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision,
            commit_id,
        };
        let event = super::super::SurfaceEventEnvelope {
            ordinal: 0,
            event_id: super::super::SurfaceEventId::try_from_bytes(uuid_v7_bytes(
                92 + u8::try_from(state.snapshot().cursor.next_seq.get()).unwrap(),
            ))
            .unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: super::super::SurfaceEvent::Session(super::super::SessionPatch::RuntimeFault {
                class: super::super::FailureClass::Persistence,
                message: super::super::DisplayText::new("projection test"),
                causative_generation: None,
            }),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: state.snapshot().cursor.clone(),
            cursor_after: super::super::SurfaceCursor {
                next_seq: super::super::SequenceNumber::new(
                    state.snapshot().cursor.next_seq.get() + 1,
                ),
                source_revision: super::super::CursorSourceRevision::Recorded { durable_revision },
                ..state.snapshot().cursor.clone()
            },
            commit_class,
            event_count: 1,
            batch_digest: digest(0),
            events: super::super::NonEmptyVec::try_new(vec![event]).unwrap(),
        };
        batch.batch_digest = super::super::canonical_batch_digest(&batch);
        batch
    }

    fn test_batch_with_events(
        state: &SurfaceReducerState,
        events: Vec<(SurfaceScope, super::super::SurfaceEvent)>,
    ) -> SurfaceCommitBatch {
        let mut batch = test_batch(state);
        let event_count = events.len() as u32;
        batch.events = super::super::NonEmptyVec::try_new(
            events
                .into_iter()
                .enumerate()
                .map(
                    |(ordinal, (scope, event))| super::super::SurfaceEventEnvelope {
                        ordinal: ordinal as u32,
                        event_id: super::super::SurfaceEventId::try_from_bytes(uuid_v7_bytes(
                            100 + u8::try_from(state.snapshot().cursor.next_seq.get()).unwrap()
                                + ordinal as u8,
                        ))
                        .unwrap(),
                        commit_class: batch.commit_class.clone(),
                        scope,
                        event,
                    },
                )
                .collect(),
        )
        .unwrap();
        batch.event_count = event_count;
        batch.cursor_after.next_seq = super::super::SequenceNumber::new(
            batch.cursor_before.next_seq.get() + event_count as u64,
        );
        batch.batch_digest = super::super::canonical_batch_digest(&batch);
        batch
    }

    fn test_operation_fence(seed: u8) -> super::super::SurfaceOperationFence {
        super::super::SurfaceOperationFence {
            thread_id: thread_id(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: super::super::SurfaceOperationId::try_from_bytes(uuid_v7_bytes(seed))
                .unwrap(),
            generation_id: super::super::SurfaceGenerationId::new(0),
        }
    }

    fn checkpoint_recovery_snapshot() -> (
        super::super::SurfaceSnapshot,
        super::super::SurfaceOperationId,
        super::super::SurfaceInteractionId,
    ) {
        let operation = started_operation();
        let operation_id = operation.operation_id.clone();
        let fence = operation.generations[0].fence.clone();
        let interaction_id =
            super::super::SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(133)).unwrap();
        let request = super::super::SurfaceInteractionRequest::UserInput {
            question: super::super::NonEmptyText::try_new("continue after restart?").unwrap(),
            suggestions: Vec::new(),
        };
        let capsule = super::super::DurableInteractionContinuationCapsule::try_new(
            interaction_id.clone(),
            fence.clone(),
            request.clone(),
            digest(134),
        )
        .unwrap();
        let recovery_disposition =
            super::super::InteractionUnavailableDisposition::restartable_continuation_turn(
                &capsule,
            )
            .unwrap();
        let interaction = super::super::SurfaceInteractionView {
            interaction_id: interaction_id.clone(),
            revision: super::super::InteractionRevision::try_new(1).unwrap(),
            fence,
            kind: super::super::SurfaceInteractionKind::UserInput,
            request,
            route: super::super::SurfaceInteractionRoute::Unassigned {
                epoch: super::super::ResponseRouteEpoch::try_new(1).unwrap(),
            },
            lifecycle: super::super::SurfaceInteractionLifecycle::Requested,
            recovery_disposition,
        };
        let mut snapshot = reducer_snapshot();
        snapshot.foreground_operation = Some(operation);
        snapshot.interactions.push(interaction);
        (snapshot, operation_id, interaction_id)
    }

    #[test]
    fn checkpoint_rejection_cancels_and_terminalizes_once() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let (snapshot, operation_id, interaction_id) = checkpoint_recovery_snapshot();
        let materialization = super::super::MaterializationCause::SameProcessProjectionReset {
            retained_incarnation: snapshot.cursor.incarnation.clone(),
        };
        let state = SurfaceReducerState::new(snapshot);
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owner_lease(TestLedger::default(), state, &owner)
                .unwrap();
        let diagnostic = super::super::SafeDiagnosticText::try_new(
            "cold recovery rejected durable interaction checkpoint: Unsafe",
        )
        .unwrap();

        assert_eq!(
            coordinator
                .recover_operation_checkpoint_rejection(
                    &operation_id,
                    &materialization,
                    super::super::GenerationExecutionFailureClass::ExternalEffectAmbiguous,
                    diagnostic.clone(),
                )
                .unwrap(),
            RecoveryAction::StopAndFinalizeRuntimeRestart
        );
        assert_eq!(coordinator.ledger().writes, 2);
        assert!(matches!(
            coordinator
                .state()
                .snapshot()
                .interactions
                .iter()
                .find(|interaction| interaction.interaction_id == interaction_id)
                .map(|interaction| &interaction.lifecycle),
            Some(super::super::SurfaceInteractionLifecycle::Cancelled {
                reason: super::super::InteractionCancelReason::CapabilityUnavailable,
            })
        ));
        let finalizing = coordinator
            .state()
            .snapshot()
            .foreground_operation
            .as_ref()
            .expect("checkpoint rejection keeps the operation visible while finalizing");
        assert!(matches!(
            finalizing.phase,
            super::super::OperationPhase::Finalizing { .. }
        ));
        assert!(matches!(
            finalizing.generations[0].stop_reason.as_ref(),
            Some(super::super::GenerationStopReason::ExecutionFailed {
                class: super::super::GenerationExecutionFailureClass::ExternalEffectAmbiguous,
                message,
            }) if message == &diagnostic && message.as_str().contains("Unsafe")
        ));

        assert_eq!(
            coordinator
                .recover_operation_checkpoint_rejection(
                    &operation_id,
                    &materialization,
                    super::super::GenerationExecutionFailureClass::ExternalEffectAmbiguous,
                    diagnostic,
                )
                .unwrap(),
            RecoveryAction::ReconcileOriginalFinalizer
        );
        assert_eq!(coordinator.ledger().writes, 4);
        let terminal = coordinator
            .state()
            .snapshot()
            .operation_history
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .expect("checkpoint rejection reaches one durable terminal");
        assert!(matches!(
            terminal.terminal.as_ref().map(|record| &record.terminal),
            Some(super::super::OperationTerminal::Failed {
                class: super::super::FailureClass::ExternalEffectAmbiguous,
                message,
            }) if message.as_str().contains("Unsafe")
        ));

        assert_eq!(
            coordinator
                .recover_operation_checkpoint_rejection(
                    &operation_id,
                    &materialization,
                    super::super::GenerationExecutionFailureClass::ExternalEffectAmbiguous,
                    super::super::SafeDiagnosticText::try_new("must not replace terminal").unwrap(),
                )
                .unwrap(),
            RecoveryAction::NoOp
        );
        assert_eq!(coordinator.ledger().writes, 4);
        assert!(matches!(
            coordinator
                .state()
                .snapshot()
                .interactions
                .iter()
                .find(|interaction| interaction.interaction_id == interaction_id)
                .map(|interaction| &interaction.lifecycle),
            Some(super::super::SurfaceInteractionLifecycle::Cancelled { .. })
        ));
    }

    #[test]
    fn retained_checkpoint_is_not_cancelled_by_unavailable_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let (snapshot, operation_id, interaction_id) = checkpoint_recovery_snapshot();
        let materialization = super::super::MaterializationCause::SameProcessProjectionReset {
            retained_incarnation: snapshot.cursor.incarnation.clone(),
        };
        let state = SurfaceReducerState::new(snapshot);
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owner_lease(TestLedger::default(), state, &owner)
                .unwrap();

        coordinator
            .recover_unavailable_interactions_except(
                &operation_id,
                &materialization,
                std::slice::from_ref(&interaction_id),
            )
            .unwrap();

        assert_eq!(coordinator.ledger().writes, 0);
        assert!(matches!(
            coordinator
                .state()
                .snapshot()
                .interactions
                .iter()
                .find(|interaction| interaction.interaction_id == interaction_id)
                .map(|interaction| &interaction.lifecycle),
            Some(super::super::SurfaceInteractionLifecycle::Requested)
        ));
    }

    fn finalization_started(
        operation_id: super::super::SurfaceOperationId,
        finalize_intent_id: super::super::SurfaceFinalizeIntentId,
    ) -> super::super::OperationPatch {
        super::super::OperationPatch::FinalizationStarted {
            operation_id,
            finalize_intent_id,
            terminal_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(110)).unwrap(),
            selected_cause: super::super::OperationFinalizationCause::Reservation(
                super::super::ReservationFinalizerReason::RuntimeRestart,
            ),
            suspended_cause: None,
            expected_settlements: Vec::new(),
        }
    }

    fn terminal_history_task(
        task_id: super::super::SurfaceTaskId,
        status: super::super::SurfaceTaskStatus,
    ) -> super::super::SurfaceTask {
        super::super::SurfaceTask {
            task_id,
            revision: super::super::TaskRevision::try_new(1).unwrap(),
            task_type: super::super::SurfaceTaskType::MainSession,
            status,
            backgrounded: false,
            description: super::super::DisplayText::new("legacy terminal history"),
            created_at: super::super::UnixMillis::new(1),
            started_at: Some(super::super::UnixMillis::new(2)),
            completed_at: Some(super::super::UnixMillis::new(3)),
            parent_operation: None,
            background_fence: None,
            workflow_run_id: None,
            subagent_id: None,
            pending_interaction_id: None,
            usage: None,
            result: Some(super::super::DisplayText::new("done")),
            error: None,
            retry_count: 0,
            output_truncated: false,
        }
    }

    fn active_task_adoption_events(
        state: &SurfaceReducerState,
        record: &LegacyActiveTaskAdoptionRecord,
        index: u8,
    ) -> Vec<(SurfaceScope, super::super::SurfaceEvent)> {
        let snapshot = state.snapshot();
        let prior_reservation_sequence = snapshot
            .foreground_operation
            .iter()
            .chain(snapshot.queued_operations.iter())
            .chain(snapshot.operation_history.iter())
            .map(|operation| operation.reservation.reservation_sequence.get())
            .max()
            .unwrap_or(0);
        let prior_operation_count = snapshot.foreground_operation.iter().count()
            + snapshot.queued_operations.len()
            + snapshot.operation_history.len();
        let seed = 140 + (u8::try_from(prior_operation_count).unwrap() + index) * 6;
        let operation_id =
            super::super::SurfaceOperationId::try_from_bytes(uuid_v7_bytes(seed)).unwrap();
        let logical_turn_id = super::super::SurfaceTurnId::new();
        let replayability = super::super::Replayability::NonReplayable {
            reason: super::super::NonReplayableReason::Missing,
            live_capsule: super::super::LiveOperationCapsule::Unavailable,
        };
        let capability_fingerprint = legacy_active_task_adoption_capability_fingerprint();
        let fence = super::super::SurfaceOperationFence {
            thread_id: snapshot.thread.thread_id.clone(),
            thread_owner_epoch: snapshot.thread.owner_epoch,
            operation_id: operation_id.clone(),
            generation_id: super::super::SurfaceGenerationId::new(0),
        };
        let generation = super::super::GenerationRecord {
            fence: fence.clone(),
            logical_turn_id: logical_turn_id.clone(),
            input: super::super::GenerationInputState::NotApplicable,
            predecessor: None,
            attempt: super::super::GenerationAttempt::Initial,
            goal_identity: None,
            replayability: replayability.clone(),
            required_capabilities: Default::default(),
            capability_fingerprint: capability_fingerprint.clone(),
            phase: super::super::GenerationPhase::Reserved,
            started_witness: None,
            stop_reason: None,
        };
        let operation = super::super::OperationRecord {
            operation_id: operation_id.clone(),
            request_id: super::super::SurfaceRequestId::try_from_bytes(uuid_v7_bytes(seed + 1))
                .unwrap(),
            intent: super::super::OperationIntent {
                origin: super::super::OperationOrigin::TuiUser,
                kind: super::super::OperationKind::UserTurn,
                initial_replayability: replayability.clone(),
                busy_disposition: super::super::BusyDisposition::Queue,
                interrupt_settlement:
                    super::super::InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: super::super::LegacyVisibility::PublishAfterAdmitted,
                settings_revision: snapshot.settings.thread_revision,
                policy_epoch: snapshot.settings.effective.policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint: capability_fingerprint.clone(),
                settings_receipt: super::super::OperationSettingsPreparationReceipt::Current {
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                },
            },
            phase: super::super::OperationPhase::Requested,
            reservation: super::super::ReservationLease::new(
                super::super::SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(seed + 2))
                    .unwrap(),
                operation_id.clone(),
                super::super::SequenceNumber::new(
                    prior_reservation_sequence + u64::from(index) + 1,
                ),
                super::super::HostIncarnation::try_from_bytes(uuid_v7_bytes(seed + 3)).unwrap(),
                super::super::MonotonicInstant {
                    clock_id: super::super::HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(
                        seed + 4,
                    ))
                    .unwrap(),
                    tick: super::super::MonotonicTick::new(0),
                },
            ),
            ready_for_admission: false,
            initial_logical_turn_id: None,
            initial_input_item_id: None,
            generations: Vec::new(),
            agent_loop_turns: Vec::new(),
            pending_control: None,
            finalization: None,
            terminal: None,
        };
        let background_fence = super::super::SurfaceBackgroundFence {
            operation_fence: fence.clone(),
            background_owner_token: super::super::SurfaceBackgroundOwnerToken::new([seed + 5; 32]),
        };
        let task_id = super::super::SurfaceTaskId::try_new(record.id()).unwrap();
        let task = super::super::SurfaceTask {
            task_id: task_id.clone(),
            revision: super::super::TaskRevision::try_new(1).unwrap(),
            task_type: super::super::SurfaceTaskType::MainSession,
            status: super::super::SurfaceTaskStatus::Running,
            backgrounded: true,
            description: super::super::DisplayText::new(record.description()),
            created_at: super::super::UnixMillis::new(record.created_at_ms()),
            started_at: record.started_at_ms().map(super::super::UnixMillis::new),
            completed_at: None,
            parent_operation: Some(operation_id.clone()),
            background_fence: Some(background_fence.clone()),
            workflow_run_id: None,
            subagent_id: None,
            pending_interaction_id: None,
            usage: None,
            result: None,
            error: None,
            retry_count: record.retry_count(),
            output_truncated: record.output_truncated(),
        };
        vec![
            (
                SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                super::super::SurfaceEvent::Operation(super::super::OperationPatch::Requested {
                    operation,
                }),
            ),
            (
                SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                super::super::SurfaceEvent::Operation(super::super::OperationPatch::Admitted {
                    operation_id: operation_id.clone(),
                    logical_turn_id,
                    input: super::super::AdmittedInput::NotApplicable,
                    first_generation: generation,
                }),
            ),
            (
                SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::GenerationStarted {
                        fence: fence.clone(),
                        witness: super::super::GenerationStartedWitness {
                            started_commit_id: next_test_commit_identity(state).1,
                            settings_revision: snapshot.settings.thread_revision,
                            policy_epoch: snapshot.settings.effective.policy_epoch,
                            durable_replayability_digest:
                                super::super::canonical_replayability_digest(&replayability),
                            capability_fingerprint,
                        },
                    },
                ),
            ),
            (
                SurfaceScope::Thread,
                super::super::SurfaceEvent::Task(super::super::TaskPatch::Upserted {
                    expected_revision: None,
                    task,
                }),
            ),
            (
                SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::GenerationTransferred {
                        fence,
                        background_fence,
                        task_id: Some(task_id),
                    },
                ),
            ),
        ]
    }

    fn active_task_adoption_batch(
        state: &SurfaceReducerState,
        records: &[LegacyActiveTaskAdoptionRecord],
    ) -> SurfaceCommitBatch {
        let events = records
            .iter()
            .enumerate()
            .flat_map(|(index, record)| {
                active_task_adoption_events(state, record, u8::try_from(index).unwrap())
            })
            .collect();
        test_batch_with_events(state, events)
    }

    fn replace_batch_events(
        batch: &mut SurfaceCommitBatch,
        mut events: Vec<super::super::SurfaceEventEnvelope>,
    ) {
        for (ordinal, event) in events.iter_mut().enumerate() {
            event.ordinal = ordinal as u32;
        }
        batch.event_count = events.len() as u32;
        batch.cursor_after.next_seq = super::super::SequenceNumber::new(
            batch.cursor_before.next_seq.get() + events.len() as u64,
        );
        batch.events = super::super::NonEmptyVec::try_new(events).unwrap();
        batch.batch_digest = super::super::canonical_batch_digest(batch);
    }

    #[test]
    fn active_task_receipt_authorizes_exact_operation_transfer_batch() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owned_lease(TestLedger::default(), state, owner)
                .unwrap();
        let registry = TaskRegistry::new_persistent(
            uuid::Uuid::from_bytes(*thread_id().as_bytes()).to_string(),
            dir.path().join("tasks"),
        )
        .unwrap();
        let legacy = registry.create_main_session("legacy active".to_string());
        registry.mark_running(&legacy.id).unwrap();

        let committed = registry
            .with_active_main_session_adoption(|receipt| {
                let batch = active_task_adoption_batch(coordinator.state(), receipt.records());
                coordinator.commit_active_task_adoption_batch(receipt, &batch)
            })
            .unwrap()
            .unwrap();

        assert!(committed.is_ok());
        assert_eq!(coordinator.state().snapshot().tasks.len(), 1);
        assert_eq!(
            coordinator.state().snapshot().background_operations.len(),
            1
        );
        assert_eq!(
            coordinator.state().snapshot().tasks[0].task_id.as_str(),
            legacy.id
        );
    }

    #[test]
    fn active_task_receipt_rejects_ambiguous_existing_operation_lineage() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owned_lease(TestLedger::default(), state, owner)
                .unwrap();
        let registry = TaskRegistry::new_persistent(
            uuid::Uuid::from_bytes(*thread_id().as_bytes()).to_string(),
            dir.path().join("tasks"),
        )
        .unwrap();
        let first = registry.create_main_session("first legacy active".to_string());
        registry.mark_running(&first.id).unwrap();
        registry
            .with_active_main_session_adoption(|receipt| {
                let batch = active_task_adoption_batch(coordinator.state(), receipt.records());
                coordinator
                    .commit_active_task_adoption_batch(receipt, &batch)
                    .unwrap();
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            coordinator.state().snapshot().background_operations.len(),
            1
        );

        let ambiguous_snapshot = coordinator.state().snapshot().clone();
        assert_eq!(ambiguous_snapshot.operation_history.len(), 1);
        let ambiguous_owner_dir = tempfile::tempdir().unwrap();
        let ambiguous_owner = ExclusiveOwnerLease::acquire_thread(
            ambiguous_owner_dir.path().join("thread.lock"),
            ambiguous_owner_dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator = RuntimeCommitCoordinator::new_with_owned_lease(
            TestLedger::default(),
            SurfaceReducerState::new(ambiguous_snapshot),
            ambiguous_owner,
        )
        .unwrap();

        let second = registry.create_main_session("ambiguous legacy active".to_string());
        registry.mark_running(&second.id).unwrap();
        let rejected = registry
            .with_active_main_session_adoption(|receipt| {
                let missing = receipt
                    .records()
                    .iter()
                    .filter(|record| record.id() == second.id)
                    .cloned()
                    .collect::<Vec<_>>();
                let batch = active_task_adoption_batch(coordinator.state(), &missing);
                coordinator.commit_active_task_adoption_batch(receipt, &batch)
            })
            .unwrap()
            .unwrap();

        assert_eq!(rejected, Err(SurfaceCommitError::StalePublisherPermit));
        assert_eq!(coordinator.state().snapshot().tasks.len(), 1);
    }

    #[test]
    fn active_task_receipt_rejects_substitution_omission_and_fence_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new_persistent(
            uuid::Uuid::from_bytes(*thread_id().as_bytes()).to_string(),
            dir.path().join("tasks"),
        )
        .unwrap();
        let first = registry.create_main_session("active a".to_string());
        let second = registry.create_main_session("active z".to_string());
        registry.mark_running(&first.id).unwrap();
        registry.mark_running(&second.id).unwrap();

        registry
            .with_active_main_session_adoption(|receipt| {
                assert_eq!(receipt.records().len(), 2);
                let state = SurfaceReducerState::new(reducer_snapshot());
                let exact = active_task_adoption_batch(&state, receipt.records());
                let mut variants = Vec::new();

                let mut substituted = exact.clone();
                let mut events = substituted.events.as_slice().to_vec();
                let super::super::SurfaceEvent::Task(super::super::TaskPatch::Upserted {
                    task,
                    ..
                }) = &mut events[3].event
                else {
                    unreachable!();
                };
                task.description = super::super::DisplayText::new("substituted");
                replace_batch_events(&mut substituted, events);
                variants.push(substituted);

                variants.push(active_task_adoption_batch(&state, &receipt.records()[..1]));

                let mut mismatched_fence = exact.clone();
                let mut events = mismatched_fence.events.as_slice().to_vec();
                let super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::GenerationTransferred { task_id, .. },
                ) = &mut events[4].event
                else {
                    unreachable!();
                };
                *task_id = None;
                replace_batch_events(&mut mismatched_fence, events);
                variants.push(mismatched_fence);

                let mut replayability = exact.clone();
                let mut events = replayability.events.as_slice().to_vec();
                let super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::Requested { operation },
                ) = &mut events[0].event
                else {
                    unreachable!();
                };
                operation.intent.initial_replayability =
                    super::super::Replayability::NonReplayable {
                        reason: super::super::NonReplayableReason::Redacted,
                        live_capsule: super::super::LiveOperationCapsule::Unavailable,
                    };
                replace_batch_events(&mut replayability, events);
                variants.push(replayability);

                let mut settings = exact.clone();
                let mut events = settings.events.as_slice().to_vec();
                let super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::Requested { operation },
                ) = &mut events[0].event
                else {
                    unreachable!();
                };
                operation.intent.settings_revision =
                    super::super::SettingsRevision::try_new(99).unwrap();
                replace_batch_events(&mut settings, events);
                variants.push(settings);

                let mut fingerprint = exact.clone();
                let mut events = fingerprint.events.as_slice().to_vec();
                let super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::GenerationStarted { witness, .. },
                ) = &mut events[2].event
                else {
                    unreachable!();
                };
                witness.capability_fingerprint = digest(222);
                replace_batch_events(&mut fingerprint, events);
                variants.push(fingerprint);

                let mut extra_event = exact.clone();
                let mut events = extra_event.events.as_slice().to_vec();
                events.push(events[0].clone());
                replace_batch_events(&mut extra_event, events);
                variants.push(extra_event);

                for (index, batch) in variants.iter().enumerate() {
                    let owner_dir = tempfile::tempdir().unwrap();
                    let owner = ExclusiveOwnerLease::acquire_thread(
                        owner_dir.path().join("thread.lock"),
                        owner_dir.path().join("thread.epoch"),
                        thread_id(),
                        &TestClock,
                    )
                    .unwrap();
                    let mut coordinator = RuntimeCommitCoordinator::new_with_owned_lease(
                        TestLedger::default(),
                        SurfaceReducerState::new(reducer_snapshot()),
                        owner,
                    )
                    .unwrap();
                    assert_eq!(
                        coordinator.commit_active_task_adoption_batch(receipt, batch),
                        Err(SurfaceCommitError::StalePublisherPermit),
                        "variant {index} unexpectedly committed"
                    );
                }
            })
            .unwrap()
            .unwrap();
    }

    #[test]
    fn actor_permit_cannot_commit_active_task_adoption_batch() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let dir = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new_persistent(
            uuid::Uuid::from_bytes(*thread_id().as_bytes()).to_string(),
            dir.path().join("tasks"),
        )
        .unwrap();
        let legacy = registry.create_main_session("legacy active".to_string());
        registry.mark_running(&legacy.id).unwrap();
        let batch = registry
            .with_active_main_session_adoption(|receipt| {
                active_task_adoption_batch(&state, receipt.records())
            })
            .unwrap()
            .unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owned_lease(TestLedger::default(), state, owner)
                .unwrap();

        assert_eq!(
            coordinator.commit_actor_batch(&batch),
            Err(SurfaceCommitError::StalePublisherPermit)
        );
    }

    #[test]
    fn actor_permit_cannot_commit_task_reconciliation() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let task = terminal_history_task(
            super::super::SurfaceTaskId::try_new("legacy-terminal").unwrap(),
            super::super::SurfaceTaskStatus::Completed,
        );
        let batch = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Thread,
                super::super::SurfaceEvent::Task(super::super::TaskPatch::Reconciled {
                    source_revision: super::super::TaskRevision::try_new(1).unwrap(),
                    tasks: vec![task],
                }),
            )],
        );
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owned_lease(TestLedger::default(), state, owner)
                .unwrap();

        assert!(matches!(
            coordinator.commit_actor_batch(&batch),
            Err(SurfaceCommitError::StalePublisherPermit)
        ));
    }

    #[test]
    fn terminal_task_receipt_authorizes_exact_append_only_batch() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owned_lease(TestLedger::default(), state, owner)
                .unwrap();
        let registry = TaskRegistry::new_persistent(
            uuid::Uuid::from_bytes(*thread_id().as_bytes()).to_string(),
            dir.path().join("tasks"),
        )
        .unwrap();
        let legacy = registry.create_main_session("legacy terminal history".to_string());
        registry.mark_running(&legacy.id).unwrap();
        registry.complete(&legacy.id, "done".to_string()).unwrap();

        let committed = registry
            .with_terminal_main_session_reconciliation(|receipt| {
                let batch = test_batch_with_events(
                    coordinator.state(),
                    vec![(
                        SurfaceScope::Thread,
                        super::super::SurfaceEvent::Task(super::super::TaskPatch::Reconciled {
                            source_revision: super::super::TaskRevision::try_new(
                                receipt.publication_horizon(),
                            )
                            .unwrap(),
                            tasks: receipt.reconciled_surface_tasks(),
                        }),
                    )],
                );
                coordinator.commit_terminal_task_reconciliation_batch(receipt, &batch)
            })
            .unwrap()
            .unwrap();

        assert!(committed.is_ok());
        assert_eq!(coordinator.state().snapshot().tasks.len(), 1);
        assert_eq!(
            coordinator.state().snapshot().tasks[0].task_id.as_str(),
            legacy.id
        );
    }

    #[test]
    fn terminal_task_receipt_rejects_substitution_omission_and_active_rows() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owned_lease(TestLedger::default(), state, owner)
                .unwrap();
        let registry = TaskRegistry::new_persistent(
            uuid::Uuid::from_bytes(*thread_id().as_bytes()).to_string(),
            dir.path().join("tasks"),
        )
        .unwrap();
        let legacy = registry.create_main_session("legacy terminal history".to_string());
        registry.mark_running(&legacy.id).unwrap();
        registry.complete(&legacy.id, "done".to_string()).unwrap();

        registry
            .with_terminal_main_session_reconciliation(|receipt| {
                let mut substituted = receipt.reconciled_surface_tasks();
                substituted[0].description = super::super::DisplayText::new("substituted");
                let substituted = test_batch_with_events(
                    coordinator.state(),
                    vec![(
                        SurfaceScope::Thread,
                        super::super::SurfaceEvent::Task(super::super::TaskPatch::Reconciled {
                            source_revision: super::super::TaskRevision::try_new(
                                receipt.publication_horizon(),
                            )
                            .unwrap(),
                            tasks: substituted,
                        }),
                    )],
                );
                assert!(matches!(
                    coordinator.commit_terminal_task_reconciliation_batch(receipt, &substituted),
                    Err(SurfaceCommitError::StalePublisherPermit)
                ));

                let omitted = test_batch_with_events(
                    coordinator.state(),
                    vec![(
                        SurfaceScope::Thread,
                        super::super::SurfaceEvent::Task(super::super::TaskPatch::Reconciled {
                            source_revision: super::super::TaskRevision::try_new(
                                receipt.publication_horizon(),
                            )
                            .unwrap(),
                            tasks: Vec::new(),
                        }),
                    )],
                );
                assert!(matches!(
                    coordinator.commit_terminal_task_reconciliation_batch(receipt, &omitted),
                    Err(SurfaceCommitError::StalePublisherPermit)
                ));

                let mut active = receipt.reconciled_surface_tasks();
                active[0].status = super::super::SurfaceTaskStatus::Running;
                active[0].completed_at = None;
                let active = test_batch_with_events(
                    coordinator.state(),
                    vec![(
                        SurfaceScope::Thread,
                        super::super::SurfaceEvent::Task(super::super::TaskPatch::Reconciled {
                            source_revision: super::super::TaskRevision::try_new(
                                receipt.publication_horizon(),
                            )
                            .unwrap(),
                            tasks: active,
                        }),
                    )],
                );
                assert!(matches!(
                    coordinator.commit_terminal_task_reconciliation_batch(receipt, &active),
                    Err(SurfaceCommitError::StalePublisherPermit)
                ));
            })
            .unwrap()
            .unwrap();
    }

    #[test]
    fn prepared_terminal_task_reconciliation_recovers_only_safe_shape() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let terminal = terminal_history_task(
            super::super::SurfaceTaskId::try_new("prepared-terminal").unwrap(),
            super::super::SurfaceTaskStatus::Stopped,
        );
        let safe = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Thread,
                super::super::SurfaceEvent::Task(super::super::TaskPatch::Reconciled {
                    source_revision: super::super::TaskRevision::try_new(1).unwrap(),
                    tasks: vec![terminal.clone()],
                }),
            )],
        );
        let mut active = terminal;
        active.status = super::super::SurfaceTaskStatus::Running;
        active.completed_at = None;
        let active = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Thread,
                super::super::SurfaceEvent::Task(super::super::TaskPatch::Reconciled {
                    source_revision: super::super::TaskRevision::try_new(1).unwrap(),
                    tasks: vec![active],
                }),
            )],
        );
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owned_lease(TestLedger::default(), state, owner)
                .unwrap();

        assert!(matches!(
            coordinator.issue_exact_recovered_authority(&safe),
            Ok(RecoveredBatchAuthority::TaskReconciliation { .. })
        ));
        assert!(matches!(
            coordinator.issue_exact_recovered_authority(&active),
            Err(SurfaceCommitError::StalePublisherPermit)
        ));
    }

    #[test]
    fn prepared_active_task_adoption_recovers_only_canonical_shape() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let dir = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new_persistent(
            uuid::Uuid::from_bytes(*thread_id().as_bytes()).to_string(),
            dir.path().join("tasks"),
        )
        .unwrap();
        let legacy = registry.create_main_session("prepared active".to_string());
        registry.mark_running(&legacy.id).unwrap();
        let canonical = registry
            .with_active_main_session_adoption(|receipt| {
                active_task_adoption_batch(&state, receipt.records())
            })
            .unwrap()
            .unwrap();

        let mut replayability = canonical.clone();
        let mut events = replayability.events.as_slice().to_vec();
        let super::super::SurfaceEvent::Operation(super::super::OperationPatch::Requested {
            operation,
        }) = &mut events[0].event
        else {
            unreachable!();
        };
        operation.intent.initial_replayability = super::super::Replayability::NonReplayable {
            reason: super::super::NonReplayableReason::SecretInput,
            live_capsule: super::super::LiveOperationCapsule::Unavailable,
        };
        replace_batch_events(&mut replayability, events);

        let mut missing_task = canonical.clone();
        let mut events = missing_task.events.as_slice().to_vec();
        events.remove(3);
        replace_batch_events(&mut missing_task, events);

        let mut wrong_fence = canonical.clone();
        let mut events = wrong_fence.events.as_slice().to_vec();
        let super::super::SurfaceEvent::Operation(
            super::super::OperationPatch::GenerationTransferred {
                background_fence, ..
            },
        ) = &mut events[4].event
        else {
            unreachable!();
        };
        background_fence.background_owner_token =
            super::super::SurfaceBackgroundOwnerToken::new([201; 32]);
        replace_batch_events(&mut wrong_fence, events);

        let mut non_main = canonical.clone();
        let mut events = non_main.events.as_slice().to_vec();
        let super::super::SurfaceEvent::Task(super::super::TaskPatch::Upserted { task, .. }) =
            &mut events[3].event
        else {
            unreachable!();
        };
        task.task_type = super::super::SurfaceTaskType::Workflow;
        replace_batch_events(&mut non_main, events);

        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owned_lease(TestLedger::default(), state, owner)
                .unwrap();
        assert!(matches!(
            coordinator.issue_exact_recovered_authority(&canonical),
            Ok(RecoveredBatchAuthority::ActiveTaskAdoption { .. })
        ));
        for batch in [replayability, missing_task, wrong_fence, non_main] {
            assert!(matches!(
                coordinator.issue_exact_recovered_authority(&batch),
                Err(SurfaceCommitError::StalePublisherPermit)
            ));
        }
    }

    #[test]
    fn actor_control_permit_cannot_publish_terminal() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let operation_id = test_operation_fence(111).operation_id;
        let finalize_intent_id =
            super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(112)).unwrap();
        let batch = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                super::super::SurfaceEvent::Operation(super::super::OperationPatch::Terminal {
                    record: super::super::OperationTerminalRecord {
                        operation_id,
                        finalize_intent_id,
                        terminal: super::super::OperationTerminal::NotAdmitted {
                            reason: super::super::NotAdmittedReason::RuntimeRestart,
                        },
                        usage: zero_usage(),
                        source_diagnostic_digest: None,
                        settlement_receipts: Vec::new(),
                        committed_at: super::super::UnixMillis::new(0),
                    },
                }),
            )],
        );
        let permit = SurfacePublisherPermit::ActorControl {
            permit_id: super::super::SurfacePublisherPermitId::new([3; 32]),
            thread_id: thread_id(),
            owner_epoch: ThreadOwnerEpoch::new(1),
        };

        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn actor_generation_interrupt_authority_is_exact_and_recoverable() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let fence = test_operation_fence(111);
        let interaction_id =
            super::super::SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(112)).unwrap();
        let batch = test_batch_with_events(
            &state,
            vec![
                (
                    SurfaceScope::Operation {
                        operation_id: fence.operation_id.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::ControlIntentCommitted {
                            operation_id: fence.operation_id.clone(),
                            request_id: super::super::SurfaceRequestId::try_from_bytes(
                                uuid_v7_bytes(113),
                            )
                            .unwrap(),
                            intent: super::super::PendingControlIntent::Interrupt {
                                generation_fence: fence.clone(),
                            },
                        },
                    ),
                ),
                (
                    SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    super::super::SurfaceEvent::Interaction(
                        super::super::InteractionPatch::Cancelled {
                            interaction_id,
                            expected_revision: super::super::InteractionRevision::try_new(1)
                                .unwrap(),
                            next_revision: super::super::InteractionRevision::try_new(2).unwrap(),
                            reason: super::super::InteractionCancelReason::OperationCancelled {
                                reason: super::super::CancelReason::User,
                            },
                        },
                    ),
                ),
            ],
        );
        let actor = SurfacePublisherPermit::ActorControl {
            permit_id: super::super::SurfacePublisherPermitId::new([14; 32]),
            thread_id: thread_id(),
            owner_epoch: ThreadOwnerEpoch::new(1),
        };
        let generation = SurfacePublisherPermit::Generation {
            permit_id: super::super::SurfacePublisherPermitId::new([15; 32]),
            fence: fence.clone(),
        };
        assert!(actor_generation_interrupt_authorized(
            &[actor.clone(), generation.clone()],
            &actor,
            &generation,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));

        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owner_lease(TestLedger::default(), state, &owner)
                .unwrap();
        assert!(matches!(
            coordinator.issue_exact_recovered_authority(&batch).unwrap(),
            RecoveredBatchAuthority::ActorGenerationInterrupt { .. }
        ));

        let other_generation = SurfacePublisherPermit::Generation {
            permit_id: super::super::SurfacePublisherPermitId::new([16; 32]),
            fence: test_operation_fence(114),
        };
        assert!(!actor_generation_interrupt_authorized(
            &[actor.clone(), other_generation.clone()],
            &actor,
            &other_generation,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn actor_control_permit_rejects_specialized_authority_classes() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let operation_fence = test_operation_fence(112);
        let operation_id = operation_fence.operation_id.clone();
        let permit = SurfacePublisherPermit::ActorControl {
            permit_id: super::super::SurfacePublisherPermitId::new([13; 32]),
            thread_id: thread_id(),
            owner_epoch: ThreadOwnerEpoch::new(1),
        };
        let finalizing = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                super::super::SurfaceEvent::Operation(finalization_started(
                    operation_id,
                    super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(113))
                        .unwrap(),
                )),
            )],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &finalizing,
            ThreadOwnerEpoch::new(1),
        ));

        let generation = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Generation {
                    fence: operation_fence,
                },
                super::super::SurfaceEvent::Session(super::super::SessionPatch::RuntimeFault {
                    class: super::super::FailureClass::Persistence,
                    message: super::super::DisplayText::new("generation authority"),
                    causative_generation: None,
                }),
            )],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &generation,
            ThreadOwnerEpoch::new(1),
        ));

        let goal_id = super::super::SurfaceGoalId::try_new("actor-control-goal").unwrap();
        let goal = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Goal {
                    goal_id: goal_id.clone(),
                    causative_generation: None,
                },
                super::super::SurfaceEvent::Goal(super::super::GoalPatchEnvelope {
                    receipt: super::super::SurfaceGoalStoreReceipt {
                        goal_id: goal_id.clone(),
                        goal_revision: super::super::GoalRevision::try_new(2).unwrap(),
                        objective_revision: super::super::GoalObjectiveRevision::new(1),
                        catalog_revision: super::super::GoalCatalogRevision::try_new(1).unwrap(),
                        goal_owner_epoch: super::super::GoalOwnerEpoch::try_new(1).unwrap(),
                        row_state: super::super::SurfaceGoalReceiptState::Removed {
                            tombstone_revision: super::super::GoalRevision::try_new(2).unwrap(),
                        },
                        store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(114))
                            .unwrap(),
                        receipt_digest: digest(114),
                    },
                    patch: super::super::GoalPatch::Removed {
                        goal_id,
                        previous_revision: super::super::GoalRevision::try_new(1).unwrap(),
                        tombstone_revision: super::super::GoalRevision::try_new(2).unwrap(),
                    },
                }),
            )],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &goal,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn actor_generation_terminalization_requires_matching_interaction_cancel_reason() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let fence = test_operation_fence(113);
        let actor = SurfacePublisherPermit::ActorControl {
            permit_id: super::super::SurfacePublisherPermitId::new([31; 32]),
            thread_id: thread_id(),
            owner_epoch: ThreadOwnerEpoch::new(1),
        };
        let generation = SurfacePublisherPermit::Generation {
            permit_id: super::super::SurfacePublisherPermitId::new([32; 32]),
            fence: fence.clone(),
        };
        let issued = vec![actor.clone(), generation.clone()];
        let cases = [
            (
                super::super::TerminalizationCause::UserCancel,
                super::super::InteractionCancelReason::OperationCancelled {
                    reason: super::super::CancelReason::User,
                },
            ),
            (
                super::super::TerminalizationCause::GoalPause,
                super::super::InteractionCancelReason::OperationCancelled {
                    reason: super::super::CancelReason::GoalPause,
                },
            ),
            (
                super::super::TerminalizationCause::HostShutdown,
                super::super::InteractionCancelReason::HostShutdown,
            ),
            (
                super::super::TerminalizationCause::ThreadClose,
                super::super::InteractionCancelReason::ThreadClose,
            ),
        ];

        for (cause_index, (cause, _)) in cases.iter().enumerate() {
            for (reason_index, (_, reason)) in cases.iter().enumerate() {
                let operation_id = fence.operation_id.clone();
                let batch = test_batch_with_events(
                    &state,
                    vec![
                        (
                            SurfaceScope::Operation {
                                operation_id: operation_id.clone(),
                            },
                            super::super::SurfaceEvent::Operation(
                                super::super::OperationPatch::ControlIntentCommitted {
                                    operation_id: operation_id.clone(),
                                    request_id: super::super::SurfaceRequestId::try_from_bytes(
                                        uuid_v7_bytes(130),
                                    )
                                    .unwrap(),
                                    intent: super::super::PendingControlIntent::Terminalize {
                                        operation_id: operation_id.clone(),
                                        cause: *cause,
                                    },
                                },
                            ),
                        ),
                        (
                            SurfaceScope::Generation {
                                fence: fence.clone(),
                            },
                            super::super::SurfaceEvent::Interaction(
                                super::super::InteractionPatch::Cancelled {
                                    interaction_id:
                                        super::super::SurfaceInteractionId::try_from_bytes(
                                            uuid_v7_bytes(131),
                                        )
                                        .unwrap(),
                                    expected_revision: super::super::InteractionRevision::try_new(
                                        1,
                                    )
                                    .unwrap(),
                                    next_revision: super::super::InteractionRevision::try_new(2)
                                        .unwrap(),
                                    reason: reason.clone(),
                                },
                            ),
                        ),
                    ],
                );

                assert_eq!(
                    actor_generation_terminalization_authorized(
                        &issued,
                        &actor,
                        &generation,
                        &batch,
                        ThreadOwnerEpoch::new(1),
                    ),
                    cause_index == reason_index,
                    "cause {cause:?} with cancellation {reason:?}",
                );
            }
        }
    }

    #[test]
    fn finalizer_permit_binds_operation_and_finalize_intent() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let operation_id = test_operation_fence(113).operation_id;
        let finalize_intent_id =
            super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(114)).unwrap();
        let batch = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                super::super::SurfaceEvent::Operation(finalization_started(
                    operation_id.clone(),
                    finalize_intent_id.clone(),
                )),
            )],
        );
        let permit = SurfacePublisherPermit::Finalizer {
            permit_id: super::super::SurfacePublisherPermitId::new([4; 32]),
            operation_id: operation_id.clone(),
            finalize_intent_id,
            owner_epoch: ThreadOwnerEpoch::new(1),
        };
        let wrong_intent = SurfacePublisherPermit::Finalizer {
            permit_id: super::super::SurfacePublisherPermitId::new([5; 32]),
            operation_id,
            finalize_intent_id: super::super::SurfaceFinalizeIntentId::try_from_bytes(
                uuid_v7_bytes(115),
            )
            .unwrap(),
            owner_epoch: ThreadOwnerEpoch::new(1),
        };

        assert!(permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        assert!(!permit_authorizes(
            std::slice::from_ref(&wrong_intent),
            &wrong_intent,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &test_batch(&state),
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn recovery_permit_binds_exact_historical_fence_and_disposition() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let historical_fence = test_operation_fence(116);
        let batch = test_batch_with_events(
            &state,
            vec![
                (
                    SurfaceScope::Generation {
                        fence: historical_fence.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::GenerationStopped {
                            fence: historical_fence.clone(),
                            reason: super::super::GenerationStopReason::RuntimeRestart,
                            usage_delta: zero_usage(),
                        },
                    ),
                ),
                (
                    SurfaceScope::Operation {
                        operation_id: historical_fence.operation_id.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::Suspended {
                            operation_id: historical_fence.operation_id.clone(),
                            cause: super::super::SuspensionCause::RecoveryRequired {
                                generation_id: historical_fence.generation_id,
                            },
                        },
                    ),
                ),
            ],
        );
        let permit = SurfacePublisherPermit::Recovery {
            permit_id: super::super::SurfacePublisherPermitId::new([6; 32]),
            current_owner_epoch: ThreadOwnerEpoch::new(1),
            historical_fence: historical_fence.clone(),
        };
        let wrong_fence = SurfacePublisherPermit::Recovery {
            permit_id: super::super::SurfacePublisherPermitId::new([7; 32]),
            current_owner_epoch: ThreadOwnerEpoch::new(1),
            historical_fence: test_operation_fence(117),
        };

        assert!(permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        assert!(!permit_authorizes(
            std::slice::from_ref(&wrong_fence),
            &wrong_fence,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        let stop_only = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Generation {
                    fence: historical_fence.clone(),
                },
                super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::GenerationStopped {
                        fence: historical_fence.clone(),
                        reason: super::super::GenerationStopReason::RuntimeRestart,
                        usage_delta: zero_usage(),
                    },
                ),
            )],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &stop_only,
            ThreadOwnerEpoch::new(1),
        ));
        let two_dispositions = test_batch_with_events(
            &state,
            vec![
                (
                    SurfaceScope::Generation {
                        fence: historical_fence.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::GenerationStopped {
                            fence: historical_fence.clone(),
                            reason: super::super::GenerationStopReason::RuntimeRestart,
                            usage_delta: zero_usage(),
                        },
                    ),
                ),
                (
                    SurfaceScope::Operation {
                        operation_id: historical_fence.operation_id.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::Suspended {
                            operation_id: historical_fence.operation_id.clone(),
                            cause: super::super::SuspensionCause::RecoveryRequired {
                                generation_id: historical_fence.generation_id,
                            },
                        },
                    ),
                ),
                (
                    SurfaceScope::Operation {
                        operation_id: historical_fence.operation_id.clone(),
                    },
                    super::super::SurfaceEvent::Operation(finalization_started(
                        historical_fence.operation_id.clone(),
                        super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(118))
                            .unwrap(),
                    )),
                ),
            ],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &two_dispositions,
            ThreadOwnerEpoch::new(1),
        ));
        let arbitrary = test_batch_with_events(
            &state,
            vec![
                (
                    SurfaceScope::Generation {
                        fence: historical_fence.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::GenerationStopped {
                            fence: historical_fence,
                            reason: super::super::GenerationStopReason::RuntimeRestart,
                            usage_delta: zero_usage(),
                        },
                    ),
                ),
                (
                    SurfaceScope::Thread,
                    super::super::SurfaceEvent::Session(super::super::SessionPatch::RuntimeFault {
                        class: super::super::FailureClass::Persistence,
                        message: super::super::DisplayText::new("arbitrary recovery write"),
                        causative_generation: None,
                    }),
                ),
            ],
        );
        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &arbitrary,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn goal_permit_binds_exact_fence_and_receipt_digest() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let goal_id = super::super::SurfaceGoalId::try_new("goal-permit").unwrap();
        let goal_fence = super::super::SurfaceGoalFence {
            goal_id: goal_id.clone(),
            goal_revision: super::super::GoalRevision::try_new(2).unwrap(),
            goal_owner_epoch: super::super::GoalOwnerEpoch::try_new(3).unwrap(),
        };
        let receipt_digest = digest(118);
        let batch = test_batch_with_events(
            &state,
            vec![(
                SurfaceScope::Goal {
                    goal_id: goal_id.clone(),
                    causative_generation: None,
                },
                super::super::SurfaceEvent::Goal(super::super::GoalPatchEnvelope {
                    receipt: super::super::SurfaceGoalStoreReceipt {
                        goal_id: goal_id.clone(),
                        goal_revision: goal_fence.goal_revision,
                        objective_revision: super::super::GoalObjectiveRevision::new(1),
                        catalog_revision: super::super::GoalCatalogRevision::try_new(1).unwrap(),
                        goal_owner_epoch: goal_fence.goal_owner_epoch,
                        row_state: super::super::SurfaceGoalReceiptState::Removed {
                            tombstone_revision: goal_fence.goal_revision,
                        },
                        store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(119))
                            .unwrap(),
                        receipt_digest: receipt_digest.clone(),
                    },
                    patch: super::super::GoalPatch::Removed {
                        goal_id,
                        previous_revision: super::super::GoalRevision::try_new(1).unwrap(),
                        tombstone_revision: goal_fence.goal_revision,
                    },
                }),
            )],
        );
        let permit = SurfacePublisherPermit::Goal {
            permit_id: super::super::SurfacePublisherPermitId::new([8; 32]),
            goal_fence: goal_fence.clone(),
            receipt_digest,
        };
        let wrong_digest = SurfacePublisherPermit::Goal {
            permit_id: super::super::SurfacePublisherPermitId::new([9; 32]),
            goal_fence,
            receipt_digest: digest(120),
        };

        assert!(permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        assert!(!permit_authorizes(
            std::slice::from_ref(&wrong_digest),
            &wrong_digest,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn recovery_permit_requires_exact_generation_stop_in_same_batch() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let mut batch = test_batch(&state);
        let operation_id =
            super::super::SurfaceOperationId::try_from_bytes(uuid_v7_bytes(94)).unwrap();
        let historical_fence = super::super::SurfaceOperationFence {
            thread_id: thread_id(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: operation_id.clone(),
            generation_id: super::super::SurfaceGenerationId::new(0),
        };
        let permit = SurfacePublisherPermit::Recovery {
            permit_id: super::super::SurfacePublisherPermitId::new([2; 32]),
            current_owner_epoch: ThreadOwnerEpoch::new(1),
            historical_fence,
        };
        batch.events =
            super::super::NonEmptyVec::try_new(vec![super::super::SurfaceEventEnvelope {
                ordinal: 0,
                event_id: super::super::SurfaceEventId::try_from_bytes(uuid_v7_bytes(95)).unwrap(),
                commit_class: batch.commit_class.clone(),
                scope: SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
                event: super::super::SurfaceEvent::Operation(
                    super::super::OperationPatch::FinalizationStarted {
                        operation_id,
                        finalize_intent_id: super::super::SurfaceFinalizeIntentId::try_from_bytes(
                            uuid_v7_bytes(96),
                        )
                        .unwrap(),
                        terminal_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(97))
                            .unwrap(),
                        selected_cause: super::super::OperationFinalizationCause::Reservation(
                            super::super::ReservationFinalizerReason::RuntimeRestart,
                        ),
                        suspended_cause: None,
                        expected_settlements: Vec::new(),
                    },
                ),
            }])
            .unwrap();
        batch.batch_digest = super::super::canonical_batch_digest(&batch);

        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn completion_only_recovery_requires_persisted_external_effect_ambiguity() {
        let fence = test_operation_fence(121);
        let tool_call_id = super::super::SurfaceToolCallId::try_new("recovery-tool").unwrap();
        let turn_id = super::super::SurfaceTurnId::new();
        let mut snapshot = reducer_snapshot();
        snapshot.tools.push(super::super::SurfaceToolView {
            request: super::super::SurfaceToolRequest {
                tool_call_id: tool_call_id.clone(),
                source_response_id: Some(
                    super::super::UuidV7::try_from_bytes(uuid_v7_bytes(122)).unwrap(),
                ),
                turn_id: turn_id.clone(),
                name: super::super::NonEmptyText::try_new("read_file").unwrap(),
                action: super::super::SurfaceToolAction::Read,
                target: Some(super::super::DisplayText::new("/tmp/input")),
                raw_arguments: super::super::DisplayText::new(r#"{"path":"/tmp/input"}"#),
                arguments_digest: digest(123),
            },
            state: super::super::SurfaceToolViewState::Running,
            invocation_started: None,
            arguments_bytes: super::super::ByteCount::new(21),
            output_bytes: super::super::ByteCount::new(0),
            streamed_output: super::super::DisplayText::new(""),
            streamed_output_truncated: false,
            result: None,
            capability_calls: vec![super::super::SurfaceCapabilityCall {
                call_id: super::super::SurfaceCapabilityCallId::try_from_bytes(uuid_v7_bytes(124))
                    .unwrap(),
                acp_session_id: super::super::NonEmptyText::try_new("session").unwrap(),
                fence: fence.clone(),
                capability_revision: super::super::CapabilityRevision::try_new(1).unwrap(),
                policy_epoch: super::super::PolicyEpoch::try_new(1).unwrap(),
                kind: super::super::SurfaceCapabilityCallKind::ReadTextFile,
                arguments_digest: digest(125),
                owning_tool_call_id: tool_call_id.clone(),
                state: super::super::SurfaceCapabilityCallState::FailedBeforeWrite {
                    error: super::super::SafeDiagnosticText::try_new("request was never written")
                        .unwrap(),
                },
            }],
            terminal_leases: Vec::new(),
        });
        let state = SurfaceReducerState::new(snapshot);
        let terminal = super::super::SurfaceToolTerminal {
            kind: super::super::SurfaceToolResultKind::ExternalEffectAmbiguous,
            source: super::super::ToolTerminalSource::Observed,
            invocation_started: super::super::ToolInvocationStarted::Yes,
        };
        let content = super::super::DisplayText::new("forged ambiguity");
        let scope = SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let batch = test_batch_with_events(
            &state,
            vec![
                (
                    scope.clone(),
                    super::super::SurfaceEvent::Tool(super::super::ToolPatch::Completed {
                        result: super::super::SurfaceToolResult {
                            tool_call_id: tool_call_id.clone(),
                            name: super::super::NonEmptyText::try_new("read_file").unwrap(),
                            terminal: terminal.clone(),
                            output: None,
                            error: Some(content.clone()),
                            exit_code: None,
                            truncated: false,
                            file_change: None,
                        },
                    }),
                ),
                (
                    scope,
                    super::super::SurfaceEvent::Item(super::super::ItemPatch::Added {
                        item: super::super::SurfaceItem::ToolResultMessage {
                            id: super::super::SurfaceItemId::new(),
                            turn_id,
                            tool_call_id,
                            content,
                            terminal,
                            pinned: false,
                        },
                    }),
                ),
            ],
        );
        let permit = SurfacePublisherPermit::Recovery {
            permit_id: super::super::SurfacePublisherPermitId::new([12; 32]),
            current_owner_epoch: ThreadOwnerEpoch::new(1),
            historical_fence: fence,
        };

        assert!(permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
        assert!(!recovery_capability_completion_matches_state(
            &state, &permit, &batch,
        ));
    }

    #[test]
    fn terminal_stream_disposition_requires_exact_open_stream_coverage() {
        let fence = test_operation_fence(130);
        let mut snapshot = reducer_snapshot();
        let first = super::super::SurfaceAssistantStream {
            stream_id: super::super::SurfaceStreamId::try_from_bytes(uuid_v7_bytes(131)).unwrap(),
            fence: fence.clone(),
            turn_id: orca_core::thread_identity::TurnId::new(),
            item_id: orca_core::thread_identity::ConversationItemId::new(),
            channel: super::super::AssistantChannel::Message,
            next_offset: super::super::ByteOffset::new(5),
            text: super::super::DisplayText::new("first"),
            state: super::super::SurfaceAssistantStreamState::Open,
        };
        let second = super::super::SurfaceAssistantStream {
            stream_id: super::super::SurfaceStreamId::try_from_bytes(uuid_v7_bytes(132)).unwrap(),
            fence: fence.clone(),
            turn_id: first.turn_id.clone(),
            item_id: orca_core::thread_identity::ConversationItemId::new(),
            channel: super::super::AssistantChannel::Reasoning,
            next_offset: super::super::ByteOffset::new(6),
            text: super::super::DisplayText::new("second"),
            state: super::super::SurfaceAssistantStreamState::Open,
        };
        snapshot.assistant_streams = vec![first.clone(), second.clone()];
        let state = SurfaceReducerState::new(snapshot);
        let scope = SurfaceScope::Generation {
            fence: fence.clone(),
        };
        let discard = |stream_id| {
            (
                scope.clone(),
                super::super::SurfaceEvent::Assistant(
                    super::super::AssistantPatch::StreamDiscarded {
                        stream_id,
                        reason: super::super::AssistantDiscardReason::ProviderFailed,
                    },
                ),
            )
        };
        let exact = test_batch_with_events(
            &state,
            vec![
                discard(first.stream_id.clone()),
                discard(second.stream_id.clone()),
            ],
        );
        let missing = test_batch_with_events(&state, vec![discard(first.stream_id.clone())]);
        let reversed = test_batch_with_events(
            &state,
            vec![discard(second.stream_id), discard(first.stream_id)],
        );

        assert!(stream_discards_cover_open_streams(
            &state,
            &fence,
            &scope,
            super::super::AssistantDiscardReason::ProviderFailed,
            exact.events.as_slice(),
        ));
        assert!(!stream_discards_cover_open_streams(
            &state,
            &fence,
            &scope,
            super::super::AssistantDiscardReason::ProviderFailed,
            missing.events.as_slice(),
        ));
        assert!(!stream_discards_cover_open_streams(
            &state,
            &fence,
            &scope,
            super::super::AssistantDiscardReason::ProviderFailed,
            reversed.events.as_slice(),
        ));
    }

    #[test]
    fn recovery_permit_rejects_live_completed_stop_and_finalizer() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let fence = test_operation_fence(121);
        let finalize_intent_id =
            super::super::SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(122)).unwrap();
        let batch = test_batch_with_events(
            &state,
            vec![
                (
                    SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    super::super::SurfaceEvent::Operation(
                        super::super::OperationPatch::GenerationStopped {
                            fence: fence.clone(),
                            reason: super::super::GenerationStopReason::Completed {
                                status: super::super::GenerationCompletionStatus::Success,
                            },
                            usage_delta: zero_usage(),
                        },
                    ),
                ),
                (
                    SurfaceScope::Operation {
                        operation_id: fence.operation_id.clone(),
                    },
                    super::super::SurfaceEvent::Operation(finalization_started(
                        fence.operation_id.clone(),
                        finalize_intent_id,
                    )),
                ),
            ],
        );
        let permit = SurfacePublisherPermit::Recovery {
            permit_id: super::super::SurfacePublisherPermitId::new([10; 32]),
            current_owner_epoch: ThreadOwnerEpoch::new(1),
            historical_fence: fence,
        };

        assert!(!permit_authorizes(
            std::slice::from_ref(&permit),
            &permit,
            &batch,
            ThreadOwnerEpoch::new(1),
        ));
    }

    #[test]
    fn projection_pending_requires_exact_retry_without_second_append() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let state = SurfaceReducerState::new(reducer_snapshot());
        let batch = test_batch(&state);
        let context = SurfaceProjectionContext {
            request_id: super::super::SurfaceRequestId::try_from_bytes(uuid_v7_bytes(93)).unwrap(),
            target: super::super::MutationTarget::Thread {
                thread_id: thread_id(),
            },
            fact_family: SurfaceFactFamily::Session,
        };
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owner_lease(TestLedger::default(), state, &owner)
                .unwrap();
        coordinator.inject_projection_failure(true);

        let error = coordinator
            .commit_actor_batch_for_projection(&context, &batch)
            .unwrap_err();
        let SurfaceCommitError::ProjectionPending { token } = error else {
            panic!("expected projection retry token");
        };
        assert_eq!(coordinator.ledger().writes, 2);
        assert!(matches!(
            coordinator.commit_actor_batch(&batch),
            Err(SurfaceCommitError::ProjectionPending { token: pending }) if pending == token
        ));
        assert_eq!(coordinator.ledger().writes, 2);

        coordinator.inject_projection_failure(false);
        coordinator.retry_projection(&token).unwrap();
        assert_eq!(coordinator.ledger().writes, 2);
        assert_eq!(coordinator.state().snapshot().cursor.next_seq.get(), 1);
    }

    #[test]
    fn interrupted_manual_compaction_context_is_reset_before_operation_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let operation_id = test_operation_fence(132).operation_id;
        let mut snapshot = reducer_snapshot();
        snapshot.context.compaction = super::super::CompactionState::Running {
            operation_id: operation_id.clone(),
            reason: super::super::CompactionReason::Manual,
            before_messages: 4,
        };
        let state = SurfaceReducerState::new(snapshot);
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owner_lease(TestLedger::default(), state, &owner)
                .unwrap();
        assert!(
            coordinator
                .recover_interrupted_manual_compaction(&operation_id, None)
                .unwrap()
        );
        assert!(matches!(
            coordinator.state().snapshot().context.compaction,
            super::super::CompactionState::Idle
        ));
        assert_eq!(
            coordinator.state().snapshot().context.revision,
            super::super::ContextRevision::try_new(2).unwrap()
        );
        assert_eq!(coordinator.ledger().writes, 2);
        assert!(
            !coordinator
                .recover_interrupted_manual_compaction(&operation_id, None)
                .unwrap()
        );
        assert_eq!(coordinator.ledger().writes, 2);
    }

    #[test]
    fn manual_compaction_item_diff_preserves_the_latest_duplicate_identity() {
        let first = super::super::SurfaceItemId::new();
        let retained_tail = super::super::SurfaceItemId::new();
        let items = vec![
            super::super::SurfaceItem::SystemMessage {
                id: first.clone(),
                content: super::super::DisplayText::new("duplicate"),
                pinned: false,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
            super::super::SurfaceItem::SystemMessage {
                id: retained_tail.clone(),
                content: super::super::DisplayText::new("duplicate"),
                pinned: false,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
        ];
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_system("duplicate".to_string());

        let patches = manual_compaction_item_patches(&items, &conversation).unwrap();

        assert_eq!(patches.len(), 3);
        assert!(matches!(
            &patches[0],
            super::super::ItemPatch::Removed { item_id, .. } if item_id == &first
        ));
        assert!(matches!(
            &patches[1],
            super::super::ItemPatch::Removed { item_id, .. } if item_id == &retained_tail
        ));
        assert!(matches!(
            &patches[2],
            super::super::ItemPatch::Added {
                item: super::super::SurfaceItem::SystemMessage { id, .. },
            } if id == &retained_tail
        ));
    }

    #[test]
    fn manual_compaction_item_rebuild_keeps_rewritten_tool_in_durable_order() {
        let prefix_id = super::super::SurfaceItemId::new();
        let tool_id = super::super::SurfaceItemId::new();
        let tail_id = super::super::SurfaceItemId::new();
        let turn_id = super::super::SurfaceTurnId::new();
        let tool_call_id = super::super::SurfaceToolCallId::try_new("call-compact").unwrap();
        let items = vec![
            super::super::SurfaceItem::SystemMessage {
                id: prefix_id.clone(),
                content: super::super::DisplayText::new("system"),
                pinned: false,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
            super::super::SurfaceItem::ToolResultMessage {
                id: tool_id.clone(),
                turn_id,
                tool_call_id: tool_call_id.clone(),
                content: super::super::DisplayText::new("large tool output"),
                terminal: super::super::SurfaceToolTerminal {
                    kind: super::super::SurfaceToolResultKind::Success,
                    source: super::super::ToolTerminalSource::Observed,
                    invocation_started: super::super::ToolInvocationStarted::Yes,
                },
                pinned: false,
            },
            super::super::SurfaceItem::SystemMessage {
                id: tail_id.clone(),
                content: super::super::DisplayText::new("tail"),
                pinned: true,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
        ];
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_system("system".to_string());
        conversation.add_tool_result(
            tool_call_id.as_str().to_string(),
            "micro-compacted tool output".to_string(),
        );
        conversation.add_system_pinned("tail".to_string());

        let patches = manual_compaction_item_patches(&items, &conversation).unwrap();

        assert_eq!(patches.len(), 6);
        assert!(matches!(
            &patches[3],
            super::super::ItemPatch::Added {
                item: super::super::SurfaceItem::SystemMessage { id, content, .. },
            } if id == &prefix_id && content.as_str() == "system"
        ));
        assert!(matches!(
            &patches[4],
            super::super::ItemPatch::Added {
                item:
                    super::super::SurfaceItem::ToolResultMessage {
                        id,
                        tool_call_id: added_call,
                        content,
                        ..
                    },
            } if id == &tool_id
                && added_call == &tool_call_id
                && content.as_str() == "micro-compacted tool output"
        ));
        assert!(matches!(
            &patches[5],
            super::super::ItemPatch::Added {
                item: super::super::SurfaceItem::SystemMessage { id, content, .. },
            } if id == &tail_id && content.as_str() == "tail"
        ));
    }

    #[test]
    fn manual_compaction_item_rebuild_preserves_identity_across_durable_redaction() {
        let item_id = super::super::SurfaceItemId::new();
        let items = vec![super::super::SurfaceItem::SystemMessage {
            id: item_id.clone(),
            content: super::super::DisplayText::new(
                "provider api_key=sk-test-manual-compact-redaction-1234567890",
            ),
            pinned: false,
            origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
        }];
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_system("provider api_key=<redacted>".to_string());

        let patches = manual_compaction_item_patches(&items, &conversation).unwrap();

        assert!(matches!(
            patches.as_slice(),
            [
                super::super::ItemPatch::Removed {
                    item_id: removed,
                    ..
                },
                super::super::ItemPatch::Added {
                    item: super::super::SurfaceItem::SystemMessage { id: added, .. },
                },
            ] if removed == &item_id && added == &item_id
        ));
    }

    #[test]
    fn manual_compaction_item_rebuild_does_not_fabricate_missing_assistant_authority() {
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_user("user".to_string());
        conversation.add_assistant(
            Some("assistant".to_string()),
            Some("reasoning".to_string()),
            Vec::new(),
        );

        let patches = manual_compaction_item_patches(&[], &conversation).unwrap();

        assert!(patches.is_empty());
    }

    #[test]
    fn manual_compaction_item_rebuild_excludes_audit_only_duplicate_user_input() {
        let resolved_id = super::super::SurfaceItemId::new();
        let failed_id = super::super::SurfaceItemId::new();
        let turn_id = super::super::SurfaceTurnId::new();
        let visible = super::super::SurfaceInputPresentation::Visible {
            text: super::super::DisplayText::new("same user text"),
        };
        let resolved = super::super::SurfaceItem::UserMessage {
            id: resolved_id.clone(),
            turn_id: turn_id.clone(),
            input: super::super::SurfaceUserInputState::Resolved {
                fact: super::super::SurfaceResolvedInputFact::NonReplayable {
                    presentation: visible.clone(),
                    live_capsule_incarnation: super::super::SurfaceIncarnation::try_from_bytes(
                        uuid_v7_bytes(145),
                    )
                    .unwrap(),
                },
            },
            pinned: false,
            origin: super::super::SurfaceItemOrigin::UserInput,
        };
        let failed = super::super::SurfaceItem::UserMessage {
            id: failed_id.clone(),
            turn_id,
            input: super::super::SurfaceUserInputState::ResolutionFailed {
                presentation: visible,
                correlation_id: super::super::SurfaceInputCorrelationId::try_from_bytes(
                    uuid_v7_bytes(146),
                )
                .unwrap(),
                code: super::super::InputResolutionErrorCode::RuntimeUnavailable,
                message: super::super::SafeDiagnosticText::try_new("resolution failed").unwrap(),
            },
            pinned: false,
            origin: super::super::SurfaceItemOrigin::UserInput,
        };
        let mut conversation = orca_core::conversation::Conversation::new();
        conversation.add_user("same user text".to_string());

        let patches =
            manual_compaction_item_patches(&[resolved.clone(), failed], &conversation).unwrap();

        assert!(matches!(
            patches.as_slice(),
            [
                super::super::ItemPatch::Removed {
                    item_id: removed_resolved,
                    ..
                },
                super::super::ItemPatch::Removed {
                    item_id: removed_failed,
                    ..
                },
                super::super::ItemPatch::Added {
                    item:
                        super::super::SurfaceItem::UserMessage {
                            id: added,
                            input: super::super::SurfaceUserInputState::Resolved { .. },
                            ..
                        },
                },
            ] if removed_resolved == &resolved_id
                && removed_failed == &failed_id
                && added == &resolved_id
        ));
    }

    #[test]
    fn interrupted_same_count_manual_compaction_recovers_from_operation_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let owner = ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            thread_id(),
            &TestClock,
        )
        .unwrap();
        let fence = test_operation_fence(133);
        let operation_id = fence.operation_id.clone();
        let mut snapshot = reducer_snapshot();
        let replayability = super::super::Replayability::NonReplayable {
            reason: super::super::NonReplayableReason::Missing,
            live_capsule: super::super::LiveOperationCapsule::Available {
                incarnation: snapshot.cursor.incarnation.clone(),
            },
        };
        let capability_fingerprint = digest(133);
        let logical_turn_id = super::super::SurfaceTurnId::new();
        snapshot.foreground_operation = Some(super::super::OperationRecord {
            operation_id: operation_id.clone(),
            request_id: super::super::SurfaceRequestId::try_from_bytes(uuid_v7_bytes(135)).unwrap(),
            intent: super::super::OperationIntent {
                origin: super::super::OperationOrigin::TuiUser,
                kind: super::super::OperationKind::ManualCompaction {
                    reason: super::super::ManualCompactionReason::Manual,
                },
                initial_replayability: replayability.clone(),
                busy_disposition: super::super::BusyDisposition::Queue,
                interrupt_settlement:
                    super::super::InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: super::super::LegacyVisibility::PublishAfterAdmitted,
                settings_revision: snapshot.settings.thread_revision,
                policy_epoch: snapshot.settings.effective.policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint: capability_fingerprint.clone(),
                settings_receipt: super::super::OperationSettingsPreparationReceipt::Current {
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                },
            },
            phase: super::super::OperationPhase::Admitted,
            reservation: super::super::ReservationLease::new(
                super::super::SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(136)).unwrap(),
                operation_id.clone(),
                super::super::SequenceNumber::new(1),
                super::super::HostIncarnation::try_from_bytes(uuid_v7_bytes(137)).unwrap(),
                super::super::MonotonicInstant {
                    clock_id: super::super::HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(
                        138,
                    ))
                    .unwrap(),
                    tick: super::super::MonotonicTick::new(0),
                },
            ),
            ready_for_admission: false,
            initial_logical_turn_id: Some(logical_turn_id.clone()),
            initial_input_item_id: None,
            generations: vec![super::super::GenerationRecord {
                fence: fence.clone(),
                logical_turn_id,
                input: super::super::GenerationInputState::NotApplicable,
                predecessor: None,
                attempt: super::super::GenerationAttempt::Initial,
                goal_identity: None,
                replayability: replayability.clone(),
                required_capabilities: Default::default(),
                capability_fingerprint: capability_fingerprint.clone(),
                phase: super::super::GenerationPhase::Started,
                started_witness: Some(super::super::GenerationStartedWitness {
                    started_commit_id: super::super::SurfaceCommitId::try_from_bytes(
                        uuid_v7_bytes(139),
                    )
                    .unwrap(),
                    settings_revision: snapshot.settings.thread_revision,
                    policy_epoch: snapshot.settings.effective.policy_epoch,
                    durable_replayability_digest: super::super::canonical_replayability_digest(
                        &replayability,
                    ),
                    capability_fingerprint,
                }),
                stop_reason: None,
            }],
            agent_loop_turns: Vec::new(),
            pending_control: None,
            finalization: None,
            terminal: None,
        });
        let compacted_item_id = super::super::SurfaceItemId::new();
        let retained_user_id = super::super::SurfaceItemId::new();
        let retained_message_id = super::super::SurfaceItemId::new();
        let retained_reasoning_id = super::super::SurfaceItemId::new();
        let retained_plan_id = super::super::SurfaceItemId::new();
        let retained_turn_id = super::super::SurfaceTurnId::new();
        snapshot.items.extend([
            super::super::SurfaceItem::SystemMessage {
                id: compacted_item_id.clone(),
                content: super::super::DisplayText::new("old context"),
                pinned: false,
                origin: super::super::SurfaceItemOrigin::HistoryMaterialization,
            },
            super::super::SurfaceItem::UserMessage {
                id: retained_user_id.clone(),
                turn_id: retained_turn_id.clone(),
                input: super::super::SurfaceUserInputState::Resolved {
                    fact: super::super::SurfaceResolvedInputFact::NonReplayable {
                        presentation: super::super::SurfaceInputPresentation::Visible {
                            text: super::super::DisplayText::new("retained user"),
                        },
                        live_capsule_incarnation: snapshot.cursor.incarnation.clone(),
                    },
                },
                pinned: false,
                origin: super::super::SurfaceItemOrigin::UserInput,
            },
            super::super::SurfaceItem::AssistantMessage {
                id: retained_message_id.clone(),
                turn_id: retained_turn_id.clone(),
                text: super::super::DisplayText::new("retained assistant"),
                pinned: false,
            },
            super::super::SurfaceItem::AssistantReasoning {
                id: retained_reasoning_id.clone(),
                turn_id: retained_turn_id.clone(),
                summary: super::super::DisplayText::new("reasoning summary"),
                content: super::super::DisplayText::new("retained reasoning"),
                pinned: false,
            },
            super::super::SurfaceItem::AssistantPlan {
                id: retained_plan_id.clone(),
                turn_id: retained_turn_id,
                text: super::super::DisplayText::new("retained plan"),
                pinned: false,
            },
        ]);
        snapshot.context.compaction = super::super::CompactionState::Running {
            operation_id: operation_id.clone(),
            reason: super::super::CompactionReason::Manual,
            before_messages: 5,
        };
        let state = SurfaceReducerState::new(snapshot);
        let mut coordinator =
            RuntimeCommitCoordinator::new_with_owner_lease(TestLedger::default(), state, &owner)
                .unwrap();
        let mut observed = orca_core::conversation::Conversation::new();
        observed.add_system(
            "[Earlier conversation history was truncated to fit context window]".to_string(),
        );
        observed.add_user("retained user".to_string());
        observed.add_assistant(
            Some("retained assistant".to_string()),
            Some("retained reasoning".to_string()),
            Vec::new(),
        );
        observed.add_assistant(Some("retained plan".to_string()), None, Vec::new());
        let durable_snapshot = crate::thread_store::ManualCompactionDurableSnapshot {
            operation_id: operation_id.clone(),
            strategy: "local_truncation".to_string(),
            before_messages: 5,
            conversation: observed,
        };

        assert!(
            coordinator
                .recover_interrupted_manual_compaction(&operation_id, Some(&durable_snapshot),)
                .unwrap()
        );
        assert!(matches!(
            &coordinator.state().snapshot().context.compaction,
            super::super::CompactionState::Completed {
                operation_id: completed,
                before_messages: 5,
                after_messages: 4,
                collapsed_messages: 1,
                ..
            } if completed == &operation_id
        ));
        assert!(matches!(
            coordinator.state().snapshot().items.as_slice(),
            [
                super::super::SurfaceItem::SystemMessage {
                    content: marker,
                    ..
                },
                super::super::SurfaceItem::UserMessage {
                    id,
                    ..
                },
                super::super::SurfaceItem::AssistantMessage {
                    id: message_id,
                    ..
                },
                super::super::SurfaceItem::AssistantReasoning {
                    id: reasoning_id,
                    ..
                },
                super::super::SurfaceItem::AssistantPlan { id: plan_id, .. },
            ] if marker.as_str()
                == "[Earlier conversation history was truncated to fit context window]"
                && id == &retained_user_id
                && message_id == &retained_message_id
                && reasoning_id == &retained_reasoning_id
                && plan_id == &retained_plan_id
        ));
        assert!(!coordinator.state().snapshot().items.iter().any(|item| {
            matches!(
                item,
                super::super::SurfaceItem::SystemMessage { id, .. }
                    if id == &compacted_item_id
            )
        }));
        assert_eq!(coordinator.ledger().writes, 2);
    }

    #[test]
    fn unbound_publication_suffix_is_latest_contiguous_and_budget_bounded() {
        let state = SurfaceReducerState::new(reducer_snapshot());
        let template = test_batch(&state);
        let mut committed = Vec::new();
        let mut cursor = template.cursor_before.clone();
        for index in 0..10_u64 {
            let mut batch = template.clone();
            batch.cursor_before = cursor.clone();
            batch.cursor_after = super::super::SurfaceCursor {
                next_seq: super::super::SequenceNumber::new(
                    cursor.next_seq.get() + super::super::SURFACE_COMMIT_BATCH_EVENT_LIMIT,
                ),
                ..cursor.clone()
            };
            batch.event_count = super::super::SURFACE_COMMIT_BATCH_EVENT_LIMIT as u32;
            batch.batch_digest = digest(index as u8);
            cursor = batch.cursor_after.clone();
            committed.push(batch);
        }
        let expected_first = committed[2].batch_digest.clone();

        let suffix = BoundedPublicationSuffix::from_committed(committed);

        assert_eq!(suffix.batches.len(), 8);
        assert_eq!(suffix.events, super::super::SURFACE_RETAINED_EVENT_LIMIT);
        assert!(suffix.bytes <= super::super::SURFACE_RETAINED_BYTE_LIMIT);
        assert_eq!(suffix.batches.front().unwrap().batch_digest, expected_first);

        let mut disconnected = Vec::new();
        let mut first = template.clone();
        first.cursor_after.next_seq = super::super::SequenceNumber::new(1);
        let mut tail_first = template.clone();
        tail_first.cursor_before.next_seq = super::super::SequenceNumber::new(5);
        tail_first.cursor_after.next_seq = super::super::SequenceNumber::new(6);
        let mut tail_second = template;
        tail_second.cursor_before = tail_first.cursor_after.clone();
        tail_second.cursor_after.next_seq = super::super::SequenceNumber::new(7);
        disconnected.extend([first, tail_first, tail_second]);

        let suffix = BoundedPublicationSuffix::from_committed(disconnected);
        assert_eq!(suffix.batches.len(), 2);
        assert_eq!(
            suffix.batches.front().unwrap().cursor_before.next_seq.get(),
            5
        );
    }
}
