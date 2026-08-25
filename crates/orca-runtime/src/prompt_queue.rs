use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const PROMPT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct QueuedSubmissionId(String);

impl QueuedSubmissionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let uuid = uuid::Uuid::parse_str(&value)
            .map_err(|error| format!("invalid queued submission id: {error}"))?;
        if uuid.get_version_num() != 7 {
            return Err("queued submission id must be UUIDv7".to_string());
        }
        Ok(Self(value))
    }
}

impl Default for QueuedSubmissionId {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de> Deserialize<'de> for QueuedSubmissionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ClientUserMessageId(String);

impl ClientUserMessageId {
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let uuid = uuid::Uuid::parse_str(&value)
            .map_err(|error| format!("invalid client user message id: {error}"))?;
        if uuid.get_version_num() != 7 {
            return Err("client user message id must be UUIDv7".to_string());
        }
        Ok(Self(value))
    }

    pub(crate) fn turn_id(&self) -> orca_core::thread_identity::TurnId {
        orca_core::thread_identity::TurnId::parse(format!("turn_{}", self.0))
            .expect("runtime queue client message ids are UUIDv7")
    }
}

impl Default for ClientUserMessageId {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de> Deserialize<'de> for ClientUserMessageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct QueueRevision(u64);

impl QueueRevision {
    pub const ZERO: Self = Self(0);

    pub fn get(self) -> u64 {
        self.0
    }

    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueuedSubmission {
    pub id: QueuedSubmissionId,
    pub client_user_message_id: ClientUserMessageId,
    pub input: PromptQueueInput,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromptQueueInput {
    pub text: String,
    #[serde(default)]
    pub mention_bindings: crate::mentions::MentionBindings,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<orca_core::conversation::ImageInput>,
}

impl PromptQueueInput {
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            mention_bindings: crate::mentions::MentionBindings::new(&text),
            text,
            images: Vec::new(),
        }
    }
}

impl From<String> for PromptQueueInput {
    fn from(value: String) -> Self {
        Self::text(value)
    }
}

impl From<&str> for PromptQueueInput {
    fn from(value: &str) -> Self {
        Self::text(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum QueueDispatchFence {
    Prepared {
        submission_id: QueuedSubmissionId,
        client_user_message_id: ClientUserMessageId,
        prepared_revision: QueueRevision,
    },
    Accepted {
        submission_id: QueuedSubmissionId,
        client_user_message_id: ClientUserMessageId,
        operation_id: String,
        accepted_revision: QueueRevision,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromptQueueSnapshot {
    pub revision: QueueRevision,
    pub paused: bool,
    pub items: Vec<QueuedSubmission>,
    pub dispatch: Option<QueueDispatchFence>,
}

impl PromptQueueSnapshot {
    /// Returns the queue head once the runtime has accepted it for execution.
    /// The item remains in `items` until terminal settlement while the runtime
    /// is live, so observers can distinguish a running head from pending work.
    pub fn running_item(&self) -> Option<&QueuedSubmission> {
        let QueueDispatchFence::Accepted { submission_id, .. } = self.dispatch.as_ref()? else {
            return None;
        };
        self.items.iter().find(|item| &item.id == submission_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptQueueAction {
    List,
    Add {
        input: PromptQueueInput,
    },
    Update {
        expected_revision: QueueRevision,
        id: QueuedSubmissionId,
        input: PromptQueueInput,
    },
    Delete {
        expected_revision: QueueRevision,
        id: QueuedSubmissionId,
    },
    Reorder {
        expected_revision: QueueRevision,
        ordered_ids: Vec<QueuedSubmissionId>,
    },
    Pause {
        expected_revision: QueueRevision,
    },
    Start {
        expected_revision: QueueRevision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptQueueMutationError {
    RevisionConflict {
        current: PromptQueueSnapshot,
    },
    NotFound {
        current: PromptQueueSnapshot,
    },
    CapacityExceeded {
        current: PromptQueueSnapshot,
    },
    DispatchInProgress {
        current: PromptQueueSnapshot,
    },
    InvalidInput {
        message: String,
        current: PromptQueueSnapshot,
    },
    PersistenceFailed {
        message: String,
    },
    RuntimeUnavailable,
}

impl std::fmt::Display for PromptQueueMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RevisionConflict { current } => write!(
                formatter,
                "prompt queue revision conflict; current revision is {}",
                current.revision.get()
            ),
            Self::NotFound { .. } => formatter.write_str("queued submission was not found"),
            Self::CapacityExceeded { .. } => formatter.write_str("prompt queue capacity exceeded"),
            Self::DispatchInProgress { .. } => {
                formatter.write_str("prompt queue dispatch is in progress")
            }
            Self::InvalidInput { message, .. } | Self::PersistenceFailed { message } => {
                formatter.write_str(message)
            }
            Self::RuntimeUnavailable => formatter.write_str("prompt queue runtime is unavailable"),
        }
    }
}

impl std::error::Error for PromptQueueMutationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptQueueState {
    snapshot: PromptQueueSnapshot,
}

impl PromptQueueState {
    pub fn from_snapshot(mut snapshot: PromptQueueSnapshot) -> Self {
        snapshot.items.truncate(PROMPT_QUEUE_CAPACITY);
        Self { snapshot }
    }

    pub fn snapshot(&self) -> PromptQueueSnapshot {
        self.snapshot.clone()
    }

    pub fn apply(
        &mut self,
        action: PromptQueueAction,
        now_unix_ms: i64,
    ) -> Result<PromptQueueSnapshot, PromptQueueMutationError> {
        match action {
            PromptQueueAction::List => return Ok(self.snapshot()),
            PromptQueueAction::Add { input } => {
                let input = normalized_input(input, &self.snapshot)?;
                if self.snapshot.items.len() >= PROMPT_QUEUE_CAPACITY {
                    return Err(PromptQueueMutationError::CapacityExceeded {
                        current: self.snapshot(),
                    });
                }
                self.snapshot.items.push(QueuedSubmission {
                    id: QueuedSubmissionId::new(),
                    client_user_message_id: ClientUserMessageId::new(),
                    input,
                    created_at_unix_ms: now_unix_ms,
                    updated_at_unix_ms: now_unix_ms,
                });
            }
            PromptQueueAction::Update {
                expected_revision,
                id,
                input,
            } => {
                self.ensure_pending_mutable(expected_revision, &id)?;
                let input = normalized_input(input, &self.snapshot)?;
                let Some(item) = self.snapshot.items.iter_mut().find(|item| item.id == id) else {
                    return Err(PromptQueueMutationError::NotFound {
                        current: self.snapshot(),
                    });
                };
                item.input = input;
                item.updated_at_unix_ms = now_unix_ms;
            }
            PromptQueueAction::Delete {
                expected_revision,
                id,
            } => {
                self.ensure_pending_mutable(expected_revision, &id)?;
                let Some(index) = self.snapshot.items.iter().position(|item| item.id == id) else {
                    return Err(PromptQueueMutationError::NotFound {
                        current: self.snapshot(),
                    });
                };
                self.snapshot.items.remove(index);
            }
            PromptQueueAction::Reorder {
                expected_revision,
                ordered_ids,
            } => {
                self.ensure_mutable(expected_revision)?;
                let current = self
                    .snapshot
                    .items
                    .iter()
                    .map(|item| item.id.clone())
                    .collect::<BTreeSet<_>>();
                let requested = ordered_ids.iter().cloned().collect::<BTreeSet<_>>();
                if current != requested || ordered_ids.len() != self.snapshot.items.len() {
                    return Err(PromptQueueMutationError::InvalidInput {
                        message: "reorder must contain every current queue id exactly once"
                            .to_string(),
                        current: self.snapshot(),
                    });
                }
                if let Some(QueueDispatchFence::Accepted { submission_id, .. }) =
                    self.snapshot.dispatch.as_ref()
                    && ordered_ids.first() != Some(submission_id)
                {
                    return Err(PromptQueueMutationError::DispatchInProgress {
                        current: self.snapshot(),
                    });
                }
                let mut remaining = std::mem::take(&mut self.snapshot.items);
                self.snapshot.items = ordered_ids
                    .into_iter()
                    .map(|id| {
                        let index = remaining
                            .iter()
                            .position(|item| item.id == id)
                            .expect("validated queue reorder id");
                        remaining.remove(index)
                    })
                    .collect();
            }
            PromptQueueAction::Pause { expected_revision } => {
                self.ensure_revision(expected_revision)?;
                self.snapshot.paused = true;
            }
            PromptQueueAction::Start { expected_revision } => {
                self.ensure_revision(expected_revision)?;
                self.snapshot.paused = false;
            }
        }
        self.snapshot.revision = self.snapshot.revision.next();
        Ok(self.snapshot())
    }

    pub(crate) fn prepare_dispatch(&mut self) -> Option<(PromptQueueSnapshot, QueuedSubmission)> {
        if self.snapshot.paused || self.snapshot.dispatch.is_some() {
            return None;
        }
        let item = self.snapshot.items.first()?.clone();
        self.snapshot.revision = self.snapshot.revision.next();
        self.snapshot.dispatch = Some(QueueDispatchFence::Prepared {
            submission_id: item.id.clone(),
            client_user_message_id: item.client_user_message_id.clone(),
            prepared_revision: self.snapshot.revision,
        });
        Some((self.snapshot(), item))
    }

    pub(crate) fn accept_dispatch(
        &mut self,
        id: &QueuedSubmissionId,
        operation_id: String,
    ) -> Option<PromptQueueSnapshot> {
        let item = self
            .snapshot
            .items
            .first()
            .filter(|item| &item.id == id)?
            .clone();
        self.snapshot.revision = self.snapshot.revision.next();
        self.snapshot.dispatch = Some(QueueDispatchFence::Accepted {
            submission_id: item.id.clone(),
            client_user_message_id: item.client_user_message_id,
            operation_id,
            accepted_revision: self.snapshot.revision,
        });
        Some(self.snapshot())
    }

    pub(crate) fn consume_accepted(&mut self) -> Option<PromptQueueSnapshot> {
        let QueueDispatchFence::Accepted { submission_id, .. } = self.snapshot.dispatch.as_ref()?
        else {
            return None;
        };
        if self.snapshot.items.first().map(|item| &item.id) != Some(submission_id) {
            return None;
        }
        self.snapshot.items.remove(0);
        self.snapshot.revision = self.snapshot.revision.next();
        self.snapshot.dispatch = None;
        Some(self.snapshot())
    }

    pub(crate) fn rollback_dispatch(&mut self) -> PromptQueueSnapshot {
        self.snapshot.revision = self.snapshot.revision.next();
        self.snapshot.dispatch = None;
        self.snapshot()
    }

    pub(crate) fn recover_dispatch(&mut self, accepted_turn: bool) -> Option<PromptQueueSnapshot> {
        match self.snapshot.dispatch.clone()? {
            QueueDispatchFence::Prepared { submission_id, .. } if accepted_turn => {
                self.accept_dispatch(&submission_id, "recovered".to_string())?;
                self.consume_accepted()
            }
            QueueDispatchFence::Prepared { .. } => Some(self.rollback_dispatch()),
            QueueDispatchFence::Accepted { .. } => self.consume_accepted(),
        }
    }

    fn ensure_revision(&self, expected: QueueRevision) -> Result<(), PromptQueueMutationError> {
        if self.snapshot.revision != expected {
            return Err(PromptQueueMutationError::RevisionConflict {
                current: self.snapshot(),
            });
        }
        Ok(())
    }

    fn ensure_mutable(&self, expected: QueueRevision) -> Result<(), PromptQueueMutationError> {
        self.ensure_revision(expected)?;
        if matches!(
            self.snapshot.dispatch,
            Some(QueueDispatchFence::Prepared { .. })
        ) {
            return Err(PromptQueueMutationError::DispatchInProgress {
                current: self.snapshot(),
            });
        }
        Ok(())
    }

    fn ensure_pending_mutable(
        &self,
        expected: QueueRevision,
        id: &QueuedSubmissionId,
    ) -> Result<(), PromptQueueMutationError> {
        self.ensure_mutable(expected)?;
        let running_id = match self.snapshot.dispatch.as_ref() {
            Some(QueueDispatchFence::Accepted { submission_id, .. }) => Some(submission_id),
            _ => None,
        };
        if running_id.is_some_and(|running_id| running_id == id) {
            return Err(PromptQueueMutationError::DispatchInProgress {
                current: self.snapshot(),
            });
        }
        Ok(())
    }
}

fn normalized_input(
    mut input: PromptQueueInput,
    current: &PromptQueueSnapshot,
) -> Result<PromptQueueInput, PromptQueueMutationError> {
    input.text = input.text.trim().to_string();
    input.mention_bindings.reconcile(&input.text);
    if input.text.is_empty() && input.images.is_empty() {
        return Err(PromptQueueMutationError::InvalidInput {
            message: "queued input must not be blank".to_string(),
            current: current.clone(),
        });
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_identifiers_validate_uuid_v7_during_deserialization_bits_spec_ut() {
        let uuid_v4 = serde_json::to_string("550e8400-e29b-41d4-a716-446655440000")
            .expect("serialize UUIDv4 string");
        assert!(
            serde_json::from_str::<QueuedSubmissionId>(&uuid_v4)
                .expect_err("queued submission ids must reject UUIDv4")
                .to_string()
                .contains("UUIDv7")
        );
        assert!(
            serde_json::from_str::<ClientUserMessageId>(&uuid_v4)
                .expect_err("client user message ids must reject UUIDv4")
                .to_string()
                .contains("UUIDv7")
        );

        let uuid_v7 = uuid::Uuid::now_v7().to_string();
        let encoded = serde_json::to_string(&uuid_v7).expect("serialize UUIDv7 string");
        assert_eq!(
            serde_json::from_str::<QueuedSubmissionId>(&encoded)
                .expect("queued submission UUIDv7")
                .as_str(),
            uuid_v7
        );
        assert_eq!(
            serde_json::from_str::<ClientUserMessageId>(&encoded)
                .expect("client user message UUIDv7")
                .as_str(),
            uuid_v7
        );
    }

    #[test]
    fn mutation_contract_preserves_identity_and_rejects_stale_revision_bits_spec_ut() {
        let mut state = PromptQueueState::from_snapshot(PromptQueueSnapshot::default());
        let added = state
            .apply(
                PromptQueueAction::Add {
                    input: "one".into(),
                },
                1,
            )
            .unwrap();
        let id = added.items[0].id.clone();
        let message_id = added.items[0].client_user_message_id.clone();
        let updated = state
            .apply(
                PromptQueueAction::Update {
                    expected_revision: added.revision,
                    id: id.clone(),
                    input: "two".into(),
                },
                2,
            )
            .unwrap();
        assert_eq!(updated.items[0].id, id);
        assert_eq!(updated.items[0].client_user_message_id, message_id);
        assert!(matches!(
            state.apply(
                PromptQueueAction::Delete {
                    expected_revision: added.revision,
                    id: updated.items[0].id.clone(),
                },
                3
            ),
            Err(PromptQueueMutationError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn image_only_input_is_not_rejected_as_blank() {
        let mut state = PromptQueueState::from_snapshot(PromptQueueSnapshot::default());
        let snapshot = state
            .apply(
                PromptQueueAction::Add {
                    input: PromptQueueInput {
                        text: String::new(),
                        mention_bindings: crate::mentions::MentionBindings::default(),
                        images: vec![orca_core::conversation::ImageInput {
                            source: orca_core::conversation::ImageSource::Url {
                                url: "https://example.com/image.png".to_string(),
                            },
                            detail: orca_core::conversation::ImageDetail::High,
                        }],
                    },
                },
                1,
            )
            .expect("queue image-only input");

        assert_eq!(snapshot.items.len(), 1);
        assert!(snapshot.items[0].input.text.is_empty());
        assert_eq!(snapshot.items[0].input.images.len(), 1);
    }

    #[test]
    fn reorder_requires_the_complete_current_identity_set_bits_spec_ut() {
        let mut state = PromptQueueState::from_snapshot(PromptQueueSnapshot::default());
        state
            .apply(
                PromptQueueAction::Add {
                    input: "one".into(),
                },
                1,
            )
            .unwrap();
        let snapshot = state
            .apply(
                PromptQueueAction::Add {
                    input: "two".into(),
                },
                2,
            )
            .unwrap();
        assert!(matches!(
            state.apply(
                PromptQueueAction::Reorder {
                    expected_revision: snapshot.revision,
                    ordered_ids: vec![snapshot.items[0].id.clone()],
                },
                3
            ),
            Err(PromptQueueMutationError::InvalidInput { .. })
        ));
    }

    #[test]
    fn prepared_fence_recovery_never_duplicates_or_loses_the_head_bits_spec_ut() {
        let mut state = PromptQueueState::from_snapshot(PromptQueueSnapshot::default());
        state
            .apply(
                PromptQueueAction::Add {
                    input: "one".into(),
                },
                1,
            )
            .unwrap();
        state.prepare_dispatch().unwrap();
        let rolled_back = state.recover_dispatch(false).unwrap();
        assert_eq!(rolled_back.items.len(), 1);
        assert!(rolled_back.dispatch.is_none());

        state.prepare_dispatch().unwrap();
        let consumed = state.recover_dispatch(true).unwrap();
        assert!(consumed.items.is_empty());
        assert!(consumed.dispatch.is_none());
    }

    #[test]
    fn accepted_dispatch_exposes_the_running_submission_until_completion_bits_spec_ut() {
        let mut state = PromptQueueState::from_snapshot(PromptQueueSnapshot::default());
        let queued = state
            .apply(
                PromptQueueAction::Add {
                    input: "running item".into(),
                },
                1,
            )
            .unwrap();
        let item = queued.items[0].clone();
        state.prepare_dispatch().expect("prepare queue head");
        let accepted = state
            .accept_dispatch(&item.id, "operation-1".to_string())
            .expect("accept queue head");

        let running = accepted
            .running_item()
            .expect("accepted item is visible as running");
        assert_eq!(running.id, item.id);
        assert_eq!(running.input.text, "running item");

        let completed = state.consume_accepted().expect("complete queue head");
        assert!(completed.running_item().is_none());
        assert!(completed.items.is_empty());
    }

    #[test]
    fn accepted_head_does_not_lock_pending_queue_mutations_bits_spec_ut() {
        let mut state = PromptQueueState::from_snapshot(PromptQueueSnapshot::default());
        state
            .apply(
                PromptQueueAction::Add {
                    input: "running item".into(),
                },
                1,
            )
            .unwrap();
        let queued = state
            .apply(
                PromptQueueAction::Add {
                    input: "pending item".into(),
                },
                2,
            )
            .unwrap();
        let running_id = queued.items[0].id.clone();
        let pending_id = queued.items[1].id.clone();
        state.prepare_dispatch().expect("prepare queue head");
        let accepted = state
            .accept_dispatch(&running_id, "operation-1".to_string())
            .expect("accept queue head");

        let updated = state
            .apply(
                PromptQueueAction::Update {
                    expected_revision: accepted.revision,
                    id: pending_id.clone(),
                    input: "edited pending item".into(),
                },
                3,
            )
            .expect("update pending item while head runs");
        assert_eq!(updated.items[1].input.text, "edited pending item");

        let deleted = state
            .apply(
                PromptQueueAction::Delete {
                    expected_revision: updated.revision,
                    id: pending_id,
                },
                4,
            )
            .expect("delete pending item while head runs");
        assert_eq!(deleted.items.len(), 1);

        assert!(matches!(
            state.apply(
                PromptQueueAction::Delete {
                    expected_revision: deleted.revision,
                    id: running_id,
                },
                5,
            ),
            Err(PromptQueueMutationError::DispatchInProgress { .. })
        ));
    }

    #[test]
    fn accepted_head_must_remain_first_during_reorder_bits_spec_ut() {
        let mut state = PromptQueueState::from_snapshot(PromptQueueSnapshot::default());
        state
            .apply(
                PromptQueueAction::Add {
                    input: "running item".into(),
                },
                1,
            )
            .unwrap();
        let queued = state
            .apply(
                PromptQueueAction::Add {
                    input: "pending item".into(),
                },
                2,
            )
            .unwrap();
        let running_id = queued.items[0].id.clone();
        let pending_id = queued.items[1].id.clone();
        state.prepare_dispatch().expect("prepare queue head");
        let accepted = state
            .accept_dispatch(&running_id, "operation-1".to_string())
            .expect("accept queue head");

        assert!(matches!(
            state.apply(
                PromptQueueAction::Reorder {
                    expected_revision: accepted.revision,
                    ordered_ids: vec![pending_id, running_id],
                },
                3,
            ),
            Err(PromptQueueMutationError::DispatchInProgress { .. })
        ));
    }
}
