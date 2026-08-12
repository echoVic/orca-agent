use super::commands::{
    AcpAttachmentCapabilityProfile, AcpStandardCapabilitySet, AttachDeniedReason, AttachResult,
    CursorAttachRequest, CursorSurfaceAttachment, DetachRequest, DetachResult,
    DetachRevocationReceipt, FreshAttachRequest, FreshSurfaceAttachment, InvalidCursor,
    InvalidCursorReason, RuntimeSurfaceClientHandle, RuntimeSurfaceCommandDispatcher,
    SURFACE_RETAINED_BYTE_LIMIT, SURFACE_RETAINED_EVENT_LIMIT, SURFACE_SUBSCRIBER_BYTE_LIMIT,
    SURFACE_SUBSCRIBER_EVENT_LIMIT, SnapshotAtCursor, SnapshotRequired, SnapshotRequiredReason,
    SurfaceAttachAuthority, SurfaceAttachmentCapabilities, SurfaceCommitBatch, SurfaceSnapshot,
    SurfaceSubscriptionHandle, SurfaceSubscriptionItem, SurfaceSubscriptionSealReason,
};
use super::identity::{
    CanonicalPath, CapabilityRevision, HostIncarnation, NonEmptySet, NonEmptyText, Sha256Digest,
    SurfaceAttachmentGrant, SurfaceAttachmentId, SurfaceAttachmentRole, SurfaceCapability,
    SurfaceCapabilityCallId, SurfaceCursor, SurfaceRemoteTerminalId, SurfaceThreadId,
    SurfaceUnavailableReason,
};
use super::interaction::SurfaceInteractionKind;
use super::projection::{SurfaceCapabilityCallKind, SurfaceTerminalExitStatus};
use super::reducer::canonical_batch_encoded_bytes;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::mpsc::{
    Receiver as SyncReceiver, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceHubConfig {
    pub retained_event_limit: u64,
    pub retained_byte_limit: u64,
    pub subscriber_event_limit: u64,
    pub subscriber_byte_limit: u64,
    pub maximum_subscribers: usize,
}

impl Default for SurfaceHubConfig {
    fn default() -> Self {
        Self {
            retained_event_limit: SURFACE_RETAINED_EVENT_LIMIT,
            retained_byte_limit: SURFACE_RETAINED_BYTE_LIMIT,
            subscriber_event_limit: SURFACE_SUBSCRIBER_EVENT_LIMIT,
            subscriber_byte_limit: SURFACE_SUBSCRIBER_BYTE_LIMIT,
            maximum_subscribers: 1_024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHubCreateError {
    RequiredCapabilitiesExceedMaximum,
    ReadSnapshotNotRequired,
    WrongThread,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceHubBindError {
    AlreadyBound,
    WrongThread,
}

#[derive(Clone)]
pub struct SurfaceHub {
    inner: Arc<Mutex<SurfaceHubState>>,
    notify: Arc<Condvar>,
    authority: SurfaceAttachAuthority,
    scope: Arc<()>,
    config: SurfaceHubConfig,
    dispatcher: Option<Arc<dyn RuntimeSurfaceCommandDispatcher>>,
}

struct SurfaceHubState {
    ready: bool,
    seal_reason: Option<SurfaceSubscriptionSealReason>,
    snapshot: Arc<SurfaceSnapshot>,
    retained: VecDeque<Arc<SharedSurfaceBatch>>,
    retained_events: u64,
    retained_bytes: u64,
    replay_hole: bool,
    subscriptions: BTreeMap<SurfaceAttachmentId, SurfaceSubscriber>,
    retired_subscriptions: BTreeMap<SurfaceAttachmentId, SurfaceSubscriber>,
}

struct SurfaceSubscriber {
    grant: SurfaceAttachmentGrant,
    interaction_kinds: BTreeSet<SurfaceInteractionKind>,
    acp_capabilities: Option<AcpAttachmentCapabilityProfile>,
    acp_read_dispatch_tx: Option<SyncSender<AcpReadTextFileDispatch>>,
    acp_read_dispatch_rx: Option<SyncReceiver<AcpReadTextFileDispatch>>,
    acp_write_dispatch_tx: Option<SyncSender<AcpWriteTextFileDispatch>>,
    acp_write_dispatch_rx: Option<SyncReceiver<AcpWriteTextFileDispatch>>,
    acp_terminal_create_dispatch_tx: Option<SyncSender<AcpTerminalCreateDispatch>>,
    acp_terminal_create_dispatch_rx: Option<SyncReceiver<AcpTerminalCreateDispatch>>,
    acp_terminal_observation_dispatch_tx: Option<SyncSender<AcpTerminalObservationDispatch>>,
    acp_terminal_observation_dispatch_rx: Option<SyncReceiver<AcpTerminalObservationDispatch>>,
    acp_terminal_cleanup_dispatch_tx: Option<SyncSender<AcpTerminalCleanupDispatch>>,
    acp_terminal_cleanup_dispatch_rx: Option<SyncReceiver<AcpTerminalCleanupDispatch>>,
    queue: VecDeque<QueuedSubscriptionItem>,
    queued_events: u64,
    queued_bytes: u64,
    claimed: bool,
    gapped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcpCapabilityAttachmentRoute {
    pub(crate) attachment_id: SurfaceAttachmentId,
    pub(crate) capability_revision: CapabilityRevision,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct AcpReadTextFileDispatch {
    pub(crate) call_id: SurfaceCapabilityCallId,
    pub(crate) acp_session_id: NonEmptyText,
    pub(crate) capability_revision: CapabilityRevision,
    pub(crate) path: CanonicalPath,
    pub(crate) line: Option<u32>,
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AcpReadTextFileSettlement {
    Completed { content: String },
    RemoteError { code: String, message: String },
    FailedBeforeWrite { message: String },
    ObservationUnavailable { message: String },
}

#[derive(Eq, PartialEq)]
pub(crate) struct AcpWriteTextFileDispatch {
    pub(crate) call_id: SurfaceCapabilityCallId,
    pub(crate) acp_session_id: NonEmptyText,
    pub(crate) capability_revision: CapabilityRevision,
    pub(crate) path: CanonicalPath,
    pub(crate) content: String,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AcpWriteTextFileSettlement {
    Completed,
    RemoteError { code: String, message: String },
    FailedBeforeWrite { message: String },
    ExternalEffectAmbiguous { message: String },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct AcpTerminalCreateDispatch {
    pub(crate) call_id: SurfaceCapabilityCallId,
    pub(crate) acp_session_id: NonEmptyText,
    pub(crate) capability_revision: CapabilityRevision,
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) cwd: Option<CanonicalPath>,
    pub(crate) output_byte_limit: Option<u64>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AcpTerminalCreateSettlement {
    Completed { terminal_id: String },
    RemoteError { code: String, message: String },
    FailedBeforeWrite { message: String },
    ExternalEffectAmbiguous { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcpTerminalObservationDispatch {
    pub(crate) call_id: SurfaceCapabilityCallId,
    pub(crate) acp_session_id: NonEmptyText,
    pub(crate) capability_revision: CapabilityRevision,
    pub(crate) terminal_id: SurfaceRemoteTerminalId,
    pub(crate) kind: SurfaceCapabilityCallKind,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AcpTerminalObservationSettlement {
    Output {
        output: String,
        truncated: bool,
        exit_status: Option<SurfaceTerminalExitStatus>,
    },
    Exit {
        exit_status: SurfaceTerminalExitStatus,
    },
    RemoteError {
        code: String,
        message: String,
    },
    FailedBeforeWrite {
        message: String,
    },
    ObservationUnavailable {
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcpTerminalCleanupDispatch {
    pub(crate) call_id: SurfaceCapabilityCallId,
    pub(crate) acp_session_id: NonEmptyText,
    pub(crate) capability_revision: CapabilityRevision,
    pub(crate) terminal_id: SurfaceRemoteTerminalId,
    pub(crate) kind: SurfaceCapabilityCallKind,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum AcpTerminalCleanupSettlement {
    Completed,
    RemoteError { code: String, message: String },
    ExternalEffectAmbiguous { message: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AcpCapabilityDispatchError {
    StaleRoute,
    Full,
    Empty,
    Disconnected,
}

pub(crate) struct AcpReadTextFileDispatchReceiver {
    receiver: SyncReceiver<AcpReadTextFileDispatch>,
}

pub(crate) struct AcpWriteTextFileDispatchReceiver {
    receiver: SyncReceiver<AcpWriteTextFileDispatch>,
}

pub(crate) struct AcpTerminalCreateDispatchReceiver {
    receiver: SyncReceiver<AcpTerminalCreateDispatch>,
}

pub(crate) struct AcpTerminalObservationDispatchReceiver {
    receiver: SyncReceiver<AcpTerminalObservationDispatch>,
}

pub(crate) struct AcpTerminalCleanupDispatchReceiver {
    receiver: SyncReceiver<AcpTerminalCleanupDispatch>,
}

impl AcpTerminalCleanupDispatchReceiver {
    pub(crate) fn try_recv(
        &self,
    ) -> Result<AcpTerminalCleanupDispatch, AcpCapabilityDispatchError> {
        match self.receiver.try_recv() {
            Ok(dispatch) => Ok(dispatch),
            Err(TryRecvError::Empty) => Err(AcpCapabilityDispatchError::Empty),
            Err(TryRecvError::Disconnected) => Err(AcpCapabilityDispatchError::Disconnected),
        }
    }
}

impl AcpTerminalCreateDispatchReceiver {
    pub(crate) fn try_recv(&self) -> Result<AcpTerminalCreateDispatch, AcpCapabilityDispatchError> {
        match self.receiver.try_recv() {
            Ok(dispatch) => Ok(dispatch),
            Err(TryRecvError::Empty) => Err(AcpCapabilityDispatchError::Empty),
            Err(TryRecvError::Disconnected) => Err(AcpCapabilityDispatchError::Disconnected),
        }
    }
}

impl AcpTerminalObservationDispatchReceiver {
    pub(crate) fn try_recv(
        &self,
    ) -> Result<AcpTerminalObservationDispatch, AcpCapabilityDispatchError> {
        match self.receiver.try_recv() {
            Ok(dispatch) => Ok(dispatch),
            Err(TryRecvError::Empty) => Err(AcpCapabilityDispatchError::Empty),
            Err(TryRecvError::Disconnected) => Err(AcpCapabilityDispatchError::Disconnected),
        }
    }
}

impl AcpWriteTextFileDispatchReceiver {
    pub(crate) fn try_recv(&self) -> Result<AcpWriteTextFileDispatch, AcpCapabilityDispatchError> {
        match self.receiver.try_recv() {
            Ok(dispatch) => Ok(dispatch),
            Err(TryRecvError::Empty) => Err(AcpCapabilityDispatchError::Empty),
            Err(TryRecvError::Disconnected) => Err(AcpCapabilityDispatchError::Disconnected),
        }
    }
}

impl AcpReadTextFileDispatchReceiver {
    pub(crate) fn try_recv(&self) -> Result<AcpReadTextFileDispatch, AcpCapabilityDispatchError> {
        match self.receiver.try_recv() {
            Ok(dispatch) => Ok(dispatch),
            Err(TryRecvError::Empty) => Err(AcpCapabilityDispatchError::Empty),
            Err(TryRecvError::Disconnected) => Err(AcpCapabilityDispatchError::Disconnected),
        }
    }
}

struct SharedSurfaceBatch {
    batch: SurfaceCommitBatch,
    encoded_bytes: u64,
}

impl SharedSurfaceBatch {
    fn new(batch: SurfaceCommitBatch) -> Self {
        let encoded_bytes = canonical_batch_encoded_bytes(&batch);
        Self {
            batch,
            encoded_bytes,
        }
    }
}

enum QueuedSubscriptionItem {
    Batch(Arc<SharedSurfaceBatch>),
    Gap(SnapshotRequired),
    Sealed(SurfaceSubscriptionSealReason),
}

pub struct SurfaceSubscriptionReceiver {
    hub: Weak<Mutex<SurfaceHubState>>,
    attachment_id: SurfaceAttachmentId,
    dispatcher: Option<Arc<dyn RuntimeSurfaceCommandDispatcher>>,
    notify: Arc<Condvar>,
}

impl SurfaceSubscriptionReceiver {
    pub fn try_recv(&mut self) -> Option<SurfaceSubscriptionItem> {
        let hub = self.hub.upgrade()?;
        let item = {
            let mut state = lock(&hub);
            take_subscription_item(&mut state, &self.attachment_id)
        };
        item.map(materialize_subscription_item)
    }

    pub fn recv_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Option<SurfaceSubscriptionItem> {
        let hub = self.hub.upgrade()?;
        let deadline = std::time::Instant::now() + timeout;
        let mut state = lock(&hub);
        loop {
            if let Some(item) = take_subscription_item(&mut state, &self.attachment_id) {
                drop(state);
                return Some(materialize_subscription_item(item));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (next, result) = self
                .notify
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
            if result.timed_out() {
                let item = take_subscription_item(&mut state, &self.attachment_id);
                drop(state);
                return item.map(materialize_subscription_item);
            }
        }
    }

    pub(crate) fn sealed_snapshot(&self) -> Option<SnapshotAtCursor> {
        let hub = self.hub.upgrade()?;
        let state = lock(&hub);
        state.seal_reason?;
        Some(SnapshotAtCursor {
            snapshot: state.snapshot.clone(),
            cursor: state.snapshot.cursor.clone(),
        })
    }
}

impl Drop for SurfaceSubscriptionReceiver {
    fn drop(&mut self) {
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let lost = {
            let mut state = lock(&hub);
            state
                .subscriptions
                .remove(&self.attachment_id)
                .or_else(|| state.retired_subscriptions.remove(&self.attachment_id))
                .map(|_| self.attachment_id.clone())
        };
        if lost.is_some() {
            if let Some(dispatcher) = self.dispatcher.as_ref() {
                dispatcher.notify_interaction_capability_changed();
            }
        }
    }
}

/// Delivers one ACP capability dispatch with a bounded retry through a
/// full lane: the client drain polls every 100ms, and the executor's
/// concurrent close() calls dispatch back-to-back, so a capacity-1 lane
/// can legitimately be full for up to one poll interval. The retry budget
/// (200ms = two drain polls) covers that race; a wedged client still fails
/// closed into the existing ambiguous settlement instead of blocking the
/// actor unboundedly. `Disconnected` never retries.
const ACP_DISPATCH_FULL_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);
const ACP_DISPATCH_FULL_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5);

fn send_acp_dispatch_retrying_full<T>(
    sender: &SyncSender<T>,
    mut dispatch: T,
) -> Result<(), AcpCapabilityDispatchError> {
    let deadline = std::time::Instant::now() + ACP_DISPATCH_FULL_RETRY_BUDGET;
    loop {
        match sender.try_send(dispatch) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                if std::time::Instant::now() >= deadline {
                    return Err(AcpCapabilityDispatchError::Full);
                }
                dispatch = returned;
                std::thread::sleep(ACP_DISPATCH_FULL_RETRY_BACKOFF);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(AcpCapabilityDispatchError::Disconnected);
            }
        }
    }
}

impl SurfaceHub {
    pub fn new_tui(
        snapshot: SurfaceSnapshot,
        host_incarnation: HostIncarnation,
        config: SurfaceHubConfig,
    ) -> Result<Self, SurfaceHubCreateError> {
        let maximum_capabilities = NonEmptySet::try_new(BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::ManageTask,
            SurfaceCapability::ManageWorkflow,
            SurfaceCapability::ManageThreadSettings,
            SurfaceCapability::ManagePinnedContext,
            SurfaceCapability::RespondGrantedInteraction,
            SurfaceCapability::RepairThread,
        ]))
        .expect("fixed TUI surface capabilities are non-empty");
        let required_capabilities =
            NonEmptySet::try_new(BTreeSet::from([SurfaceCapability::ReadSnapshot]))
                .expect("fixed TUI required capabilities are non-empty");
        let authority = SurfaceAttachAuthority::new(
            host_incarnation.clone(),
            snapshot.thread.thread_id.clone(),
            SurfaceAttachmentRole::Tui,
            maximum_capabilities,
            required_capabilities,
            BTreeSet::from([
                SurfaceInteractionKind::ToolApproval,
                SurfaceInteractionKind::PermissionRequest,
                SurfaceInteractionKind::UserInput,
                SurfaceInteractionKind::McpElicitation,
            ]),
        );
        Self::from_authority(snapshot, authority, config)
    }

    pub(crate) fn from_authority(
        snapshot: SurfaceSnapshot,
        authority: SurfaceAttachAuthority,
        config: SurfaceHubConfig,
    ) -> Result<Self, SurfaceHubCreateError> {
        let maximum_capabilities = authority.maximum_capabilities();
        let required_capabilities = authority.required_capabilities();
        if !required_capabilities
            .as_set()
            .is_subset(maximum_capabilities.as_set())
        {
            return Err(SurfaceHubCreateError::RequiredCapabilitiesExceedMaximum);
        }
        if !required_capabilities
            .as_set()
            .contains(&SurfaceCapability::ReadSnapshot)
        {
            return Err(SurfaceHubCreateError::ReadSnapshotNotRequired);
        }
        if authority.thread_id() != &snapshot.thread.thread_id
            || authority.thread_id() != &snapshot.cursor.thread_id
        {
            return Err(SurfaceHubCreateError::WrongThread);
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(SurfaceHubState {
                ready: false,
                seal_reason: None,
                snapshot: Arc::new(snapshot),
                retained: VecDeque::new(),
                retained_events: 0,
                retained_bytes: 0,
                replay_hole: false,
                subscriptions: BTreeMap::new(),
                retired_subscriptions: BTreeMap::new(),
            })),
            notify: Arc::new(Condvar::new()),
            authority,
            scope: Arc::new(()),
            config,
            dispatcher: None,
        })
    }

    pub(crate) fn with_dispatcher(
        mut self,
        dispatcher: Arc<dyn RuntimeSurfaceCommandDispatcher>,
    ) -> Self {
        self.dispatcher = Some(dispatcher);
        self
    }

    pub(crate) fn with_authority(
        &self,
        authority: SurfaceAttachAuthority,
    ) -> Result<Self, SurfaceHubCreateError> {
        let maximum_capabilities = authority.maximum_capabilities();
        let required_capabilities = authority.required_capabilities();
        if !required_capabilities
            .as_set()
            .is_subset(maximum_capabilities.as_set())
        {
            return Err(SurfaceHubCreateError::RequiredCapabilitiesExceedMaximum);
        }
        if !required_capabilities
            .as_set()
            .contains(&SurfaceCapability::ReadSnapshot)
        {
            return Err(SurfaceHubCreateError::ReadSnapshotNotRequired);
        }
        if authority.thread_id() != &self.thread_id()
            || authority.host_incarnation() != self.authority.host_incarnation()
        {
            return Err(SurfaceHubCreateError::WrongThread);
        }
        Ok(Self {
            inner: self.inner.clone(),
            notify: self.notify.clone(),
            authority,
            scope: self.scope.clone(),
            config: self.config,
            dispatcher: self.dispatcher.clone(),
        })
    }

    pub fn attach_fresh(&self, request: FreshAttachRequest) -> AttachResult {
        self.attach_fresh_with_acp_capabilities(request, None)
    }

    pub(crate) fn attach_acp_fresh(
        &self,
        request: FreshAttachRequest,
        capability_profile: AcpAttachmentCapabilityProfile,
    ) -> AttachResult {
        if self.authority.connection_id().is_none() {
            return AttachResult::Unavailable {
                reason: SurfaceUnavailableReason::RuntimeUnavailable,
            };
        }
        if request.role != SurfaceAttachmentRole::Acp {
            return AttachResult::Denied {
                reason: AttachDeniedReason::RoleMismatch,
            };
        }
        self.attach_fresh_with_acp_capabilities(request, Some(capability_profile))
    }

    fn attach_fresh_with_acp_capabilities(
        &self,
        request: FreshAttachRequest,
        capability_profile: Option<AcpAttachmentCapabilityProfile>,
    ) -> AttachResult {
        let mut state = lock(&self.inner);
        if !state.ready {
            return AttachResult::Unavailable {
                reason: SurfaceUnavailableReason::RuntimeUnavailable,
            };
        }
        let mut capabilities = match self.authorize(
            request.role,
            &request.requested_capabilities,
            &request.interaction_capabilities,
            &state.snapshot.cursor,
        ) {
            Ok(capabilities) => capabilities,
            Err(reason) => return AttachResult::Denied { reason },
        };
        capabilities.acp_capability_revision =
            capability_profile.map(|capabilities| capabilities.revision);
        if state.snapshot.thread.closed {
            return AttachResult::ThreadClosed {
                thread_id: state.snapshot.thread.thread_id.clone(),
            };
        }
        if !has_registration_capacity(&state, self.config) {
            return AttachResult::Unavailable {
                reason: SurfaceUnavailableReason::CapacityExceeded,
            };
        }
        let baseline = SnapshotAtCursor {
            snapshot: state.snapshot.clone(),
            cursor: state.snapshot.cursor.clone(),
        };
        let (attachment_id, client, subscription, capabilities) =
            self.register(&mut state, capabilities, capability_profile);
        AttachResult::FreshAttached {
            attachment: FreshSurfaceAttachment {
                attachment_id,
                client,
                baseline,
                subscription,
                capabilities,
            },
        }
    }

    pub fn attach_after(&self, request: CursorAttachRequest) -> AttachResult {
        let (attachment_id, client, from, head, replay, subscription, capabilities) = {
            let mut state = lock(&self.inner);
            if !state.ready {
                return AttachResult::Unavailable {
                    reason: SurfaceUnavailableReason::RuntimeUnavailable,
                };
            }
            let head = state.snapshot.cursor.clone();
            let capabilities = match self.authorize(
                request.role,
                &request.requested_capabilities,
                &request.interaction_capabilities,
                &head,
            ) {
                Ok(capabilities) => capabilities,
                Err(reason) => return AttachResult::Denied { reason },
            };
            if state.snapshot.thread.closed {
                return AttachResult::ThreadClosed {
                    thread_id: state.snapshot.thread.thread_id.clone(),
                };
            }
            if !has_registration_capacity(&state, self.config) {
                return AttachResult::Unavailable {
                    reason: SurfaceUnavailableReason::CapacityExceeded,
                };
            }
            let replay = match capture_retained_replay(&state, &request.cursor) {
                Ok(replay) => replay,
                Err(result) => return result,
            };
            let from = request.cursor;
            let (attachment_id, client, subscription, capabilities) =
                self.register(&mut state, capabilities, None);
            (
                attachment_id,
                client,
                from,
                head,
                replay,
                subscription,
                capabilities,
            )
        };
        let replay = materialize_replay(replay);
        AttachResult::CursorAttached {
            attachment: CursorSurfaceAttachment {
                attachment_id,
                client,
                from,
                head,
                replay,
                subscription,
                capabilities,
            },
        }
    }

    pub fn claim_subscription(
        &self,
        handle: &SurfaceSubscriptionHandle,
    ) -> Option<SurfaceSubscriptionReceiver> {
        let attachment_id = handle.attachment_id();
        let mut state = lock(&self.inner);
        let subscriber = subscriber_mut(&mut state, attachment_id)?;
        if subscriber.claimed {
            return None;
        }
        subscriber.claimed = true;
        Some(SurfaceSubscriptionReceiver {
            hub: Arc::downgrade(&self.inner),
            attachment_id: attachment_id.clone(),
            dispatcher: self.dispatcher.clone(),
            notify: self.notify.clone(),
        })
    }

    pub(crate) fn select_acp_capability_attachment(
        &self,
        kind: SurfaceCapabilityCallKind,
        origin: &SurfaceAttachmentId,
    ) -> Option<AcpCapabilityAttachmentRoute> {
        let state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return None;
        }
        state
            .subscriptions
            .iter()
            .filter(|(attachment_id, subscriber)| {
                origin == *attachment_id
                    && subscriber.claimed
                    && subscriber.grant.role == SurfaceAttachmentRole::Acp
                    && subscriber.acp_capabilities.is_some_and(|capabilities| {
                        acp_standard_capability_supports(capabilities.standard, kind)
                    })
            })
            .filter_map(|(attachment_id, subscriber)| {
                subscriber
                    .acp_capabilities
                    .map(|capabilities| AcpCapabilityAttachmentRoute {
                        attachment_id: attachment_id.clone(),
                        capability_revision: capabilities.revision,
                    })
            })
            .next()
    }

    pub(crate) fn claim_acp_read_text_file_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpReadTextFileDispatchReceiver> {
        let mut state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return None;
        }
        if !client.belongs_to(
            &self.scope,
            &state.snapshot.thread.thread_id,
            self.authority.host_incarnation(),
        ) || client.detached_receipt().is_some()
        {
            return None;
        }
        let subscriber = state.subscriptions.get_mut(client.attachment_id())?;
        if !subscriber.claimed
            || &subscriber.grant != client.grant()
            || !subscriber
                .acp_capabilities
                .is_some_and(|profile| profile.standard.file_read)
        {
            return None;
        }
        subscriber
            .acp_read_dispatch_rx
            .take()
            .map(|receiver| AcpReadTextFileDispatchReceiver { receiver })
    }

    pub(crate) fn dispatch_acp_read_text_file(
        &self,
        route: &AcpCapabilityAttachmentRoute,
        dispatch: AcpReadTextFileDispatch,
    ) -> Result<(), AcpCapabilityDispatchError> {
        let state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let subscriber = state
            .subscriptions
            .get(&route.attachment_id)
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        if !subscriber.claimed
            || subscriber.grant.role != SurfaceAttachmentRole::Acp
            || dispatch.capability_revision != route.capability_revision
            || !subscriber.acp_capabilities.is_some_and(|profile| {
                profile.revision == route.capability_revision && profile.standard.file_read
            })
        {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let sender = subscriber
            .acp_read_dispatch_tx
            .clone()
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        drop(state);
        send_acp_dispatch_retrying_full(&sender, dispatch)
    }

    pub(crate) fn claim_acp_write_text_file_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpWriteTextFileDispatchReceiver> {
        let mut state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return None;
        }
        if !client.belongs_to(
            &self.scope,
            &state.snapshot.thread.thread_id,
            self.authority.host_incarnation(),
        ) || client.detached_receipt().is_some()
        {
            return None;
        }
        let subscriber = state.subscriptions.get_mut(client.attachment_id())?;
        if !subscriber.claimed
            || &subscriber.grant != client.grant()
            || !subscriber
                .acp_capabilities
                .is_some_and(|profile| profile.standard.file_write)
        {
            return None;
        }
        subscriber
            .acp_write_dispatch_rx
            .take()
            .map(|receiver| AcpWriteTextFileDispatchReceiver { receiver })
    }

    pub(crate) fn dispatch_acp_write_text_file(
        &self,
        route: &AcpCapabilityAttachmentRoute,
        dispatch: AcpWriteTextFileDispatch,
    ) -> Result<(), AcpCapabilityDispatchError> {
        let state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let subscriber = state
            .subscriptions
            .get(&route.attachment_id)
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        if !subscriber.claimed
            || subscriber.grant.role != SurfaceAttachmentRole::Acp
            || dispatch.capability_revision != route.capability_revision
            || !subscriber.acp_capabilities.is_some_and(|profile| {
                profile.revision == route.capability_revision && profile.standard.file_write
            })
        {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let sender = subscriber
            .acp_write_dispatch_tx
            .clone()
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        drop(state);
        send_acp_dispatch_retrying_full(&sender, dispatch)
    }

    pub(crate) fn claim_acp_terminal_create_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpTerminalCreateDispatchReceiver> {
        let mut state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return None;
        }
        if !client.belongs_to(
            &self.scope,
            &state.snapshot.thread.thread_id,
            self.authority.host_incarnation(),
        ) || client.detached_receipt().is_some()
        {
            return None;
        }
        let subscriber = state.subscriptions.get_mut(client.attachment_id())?;
        if !subscriber.claimed
            || &subscriber.grant != client.grant()
            || !subscriber
                .acp_capabilities
                .is_some_and(|profile| profile.standard.terminal)
        {
            return None;
        }
        subscriber
            .acp_terminal_create_dispatch_rx
            .take()
            .map(|receiver| AcpTerminalCreateDispatchReceiver { receiver })
    }

    pub(crate) fn dispatch_acp_terminal_create(
        &self,
        route: &AcpCapabilityAttachmentRoute,
        dispatch: AcpTerminalCreateDispatch,
    ) -> Result<(), AcpCapabilityDispatchError> {
        let state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let subscriber = state
            .subscriptions
            .get(&route.attachment_id)
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        if !subscriber.claimed
            || subscriber.grant.role != SurfaceAttachmentRole::Acp
            || dispatch.capability_revision != route.capability_revision
            || !subscriber.acp_capabilities.is_some_and(|profile| {
                profile.revision == route.capability_revision && profile.standard.terminal
            })
        {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let sender = subscriber
            .acp_terminal_create_dispatch_tx
            .clone()
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        drop(state);
        send_acp_dispatch_retrying_full(&sender, dispatch)
    }

    pub(crate) fn claim_acp_terminal_observation_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpTerminalObservationDispatchReceiver> {
        let mut state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return None;
        }
        if !client.belongs_to(
            &self.scope,
            &state.snapshot.thread.thread_id,
            self.authority.host_incarnation(),
        ) || client.detached_receipt().is_some()
        {
            return None;
        }
        let subscriber = state.subscriptions.get_mut(client.attachment_id())?;
        if !subscriber.claimed
            || &subscriber.grant != client.grant()
            || !subscriber
                .acp_capabilities
                .is_some_and(|profile| profile.standard.terminal)
        {
            return None;
        }
        subscriber
            .acp_terminal_observation_dispatch_rx
            .take()
            .map(|receiver| AcpTerminalObservationDispatchReceiver { receiver })
    }

    pub(crate) fn dispatch_acp_terminal_observation(
        &self,
        route: &AcpCapabilityAttachmentRoute,
        dispatch: AcpTerminalObservationDispatch,
    ) -> Result<(), AcpCapabilityDispatchError> {
        let state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let subscriber = state
            .subscriptions
            .get(&route.attachment_id)
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        if !subscriber.claimed
            || subscriber.grant.role != SurfaceAttachmentRole::Acp
            || dispatch.capability_revision != route.capability_revision
            || !subscriber.acp_capabilities.is_some_and(|profile| {
                profile.revision == route.capability_revision && profile.standard.terminal
            })
            || !matches!(
                dispatch.kind,
                SurfaceCapabilityCallKind::TerminalOutput
                    | SurfaceCapabilityCallKind::TerminalWaitForExit
            )
        {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let sender = subscriber
            .acp_terminal_observation_dispatch_tx
            .clone()
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        drop(state);
        send_acp_dispatch_retrying_full(&sender, dispatch)
    }

    pub(crate) fn claim_acp_terminal_cleanup_dispatch(
        &self,
        client: &RuntimeSurfaceClientHandle,
    ) -> Option<AcpTerminalCleanupDispatchReceiver> {
        let mut state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return None;
        }
        if !client.belongs_to(
            &self.scope,
            &state.snapshot.thread.thread_id,
            self.authority.host_incarnation(),
        ) || client.detached_receipt().is_some()
        {
            return None;
        }
        let subscriber = state.subscriptions.get_mut(client.attachment_id())?;
        if !subscriber.claimed
            || &subscriber.grant != client.grant()
            || !subscriber
                .acp_capabilities
                .is_some_and(|profile| profile.standard.terminal)
        {
            return None;
        }
        subscriber
            .acp_terminal_cleanup_dispatch_rx
            .take()
            .map(|receiver| AcpTerminalCleanupDispatchReceiver { receiver })
    }

    pub(crate) fn dispatch_acp_terminal_cleanup(
        &self,
        route: &AcpCapabilityAttachmentRoute,
        dispatch: AcpTerminalCleanupDispatch,
    ) -> Result<(), AcpCapabilityDispatchError> {
        let state = lock(&self.inner);
        if !state.ready || state.seal_reason.is_some() {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let subscriber = state
            .subscriptions
            .get(&route.attachment_id)
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        if !subscriber.claimed
            || subscriber.grant.role != SurfaceAttachmentRole::Acp
            || dispatch.capability_revision != route.capability_revision
            || !subscriber.acp_capabilities.is_some_and(|profile| {
                profile.revision == route.capability_revision && profile.standard.terminal
            })
            || !matches!(
                dispatch.kind,
                SurfaceCapabilityCallKind::TerminalKill
                    | SurfaceCapabilityCallKind::TerminalRelease
            )
        {
            return Err(AcpCapabilityDispatchError::StaleRoute);
        }
        let sender = subscriber
            .acp_terminal_cleanup_dispatch_tx
            .clone()
            .ok_or(AcpCapabilityDispatchError::StaleRoute)?;
        drop(state);
        send_acp_dispatch_retrying_full(&sender, dispatch)
    }

    pub(crate) fn apply_committed(
        &self,
        snapshot: Arc<SurfaceSnapshot>,
        batch: &SurfaceCommitBatch,
    ) {
        let batch = Arc::new(SharedSurfaceBatch::new(batch.clone()));
        let lost = {
            let mut state = lock(&self.inner);
            if state.snapshot.cursor != batch.batch.cursor_before
                || snapshot.cursor != batch.batch.cursor_after
            {
                let lost = signal_gap(
                    &mut state,
                    SnapshotRequiredReason::ReplayHole,
                    snapshot.cursor.clone(),
                );
                state.snapshot = snapshot;
                clear_retained(&mut state);
                state.replay_hole = true;
                lost
            } else {
                retain_batch(&mut state, batch.clone(), self.config);
                let lost = publish_batch(&mut state, batch, self.config);
                state.snapshot = snapshot;
                lost
            }
        };
        self.notify.notify_all();
        self.notify_interaction_capability_change(!lost.is_empty());
    }

    pub(crate) fn repair_committed(
        &self,
        snapshot: Arc<SurfaceSnapshot>,
        committed: &[SurfaceCommitBatch],
    ) {
        let committed = committed
            .iter()
            .cloned()
            .map(SharedSurfaceBatch::new)
            .map(Arc::new)
            .collect::<Vec<_>>();
        let lost = {
            let mut state = lock(&self.inner);
            let mut lost = Vec::new();
            if state.snapshot.cursor == snapshot.cursor {
                if state.retained.is_empty() {
                    let mut expected = snapshot.cursor.clone();
                    let mut suffix = Vec::new();
                    for batch in committed.iter().rev() {
                        if batch.batch.cursor_after == expected {
                            suffix.push(batch);
                            expected = batch.batch.cursor_before.clone();
                        }
                    }
                    for batch in suffix.into_iter().rev() {
                        retain_batch(&mut state, batch.clone(), self.config);
                    }
                }
            } else {
                let mut current = state.snapshot.cursor.clone();
                let mut repaired_any = false;
                for batch in committed {
                    if batch.batch.cursor_after.next_seq.get() <= current.next_seq.get() {
                        continue;
                    }
                    if batch.batch.cursor_before != current {
                        continue;
                    }
                    retain_batch(&mut state, batch.clone(), self.config);
                    lost.extend(publish_batch(&mut state, batch.clone(), self.config));
                    current = batch.batch.cursor_after.clone();
                    repaired_any = true;
                    if current == snapshot.cursor {
                        break;
                    }
                }
                if !repaired_any || current != snapshot.cursor {
                    lost.extend(signal_gap(
                        &mut state,
                        SnapshotRequiredReason::ReplayHole,
                        snapshot.cursor.clone(),
                    ));
                    clear_retained(&mut state);
                    state.replay_hole = true;
                }
            }
            state.snapshot = snapshot;
            state.ready = true;
            lost
        };
        self.notify.notify_all();
        self.notify_interaction_capability_change(!lost.is_empty());
    }

    pub(crate) fn seal_subscriptions(&self, reason: SurfaceSubscriptionSealReason) {
        let changed = {
            let mut state = lock(&self.inner);
            if state.seal_reason.is_some() {
                false
            } else {
                state.seal_reason = Some(reason);
                state.ready = false;
                for subscriber in state.subscriptions.values_mut() {
                    subscriber
                        .queue
                        .push_back(QueuedSubscriptionItem::Sealed(reason));
                }
                true
            }
        };
        if changed {
            self.notify.notify_all();
            self.notify_interaction_capability_change(true);
        }
    }

    fn notify_interaction_capability_change(&self, changed: bool) {
        if changed {
            if let Some(dispatcher) = self.dispatcher.as_ref() {
                dispatcher.notify_interaction_capability_changed();
            }
        }
    }

    pub fn detach(
        &self,
        client: &RuntimeSurfaceClientHandle,
        request: DetachRequest,
    ) -> DetachResult {
        if let Some(dispatcher) = self.dispatcher.as_ref() {
            return dispatcher.detach(client.clone(), request);
        }
        self.detach_local(client, request)
    }

    pub(crate) fn detach_local(
        &self,
        client: &RuntimeSurfaceClientHandle,
        request: DetachRequest,
    ) -> DetachResult {
        match self.prepare_detach_local(client, request) {
            DetachResult::Detached { receipt } => self.finalize_detach_local(client, receipt),
            other => other,
        }
    }

    pub(crate) fn prepare_detach_local(
        &self,
        client: &RuntimeSurfaceClientHandle,
        request: DetachRequest,
    ) -> DetachResult {
        let state = lock(&self.inner);
        let attachment_id = client.attachment_id();
        if !client.belongs_to(
            &self.scope,
            &state.snapshot.thread.thread_id,
            self.authority.host_incarnation(),
        ) {
            return DetachResult::StaleAttachment {
                request_id: request.request_id,
                attachment_id: attachment_id.clone(),
            };
        }
        if let Some(receipt) = client.detached_receipt() {
            return DetachResult::AlreadyDetached { receipt };
        }
        let grant = state
            .subscriptions
            .get(attachment_id)
            .or_else(|| state.retired_subscriptions.get(attachment_id))
            .map(|subscriber| subscriber.grant.clone())
            .unwrap_or_else(|| client.grant().clone());
        let receipt = DetachRevocationReceipt {
            request_id: request.request_id,
            attachment_id: attachment_id.clone(),
            revoked_grant_digest: grant_digest(&grant),
            affected_route_epochs: Vec::new(),
            route_commit_id: None,
            route_cursor: None,
        };
        DetachResult::Detached { receipt }
    }

    pub(crate) fn finalize_detach_local(
        &self,
        client: &RuntimeSurfaceClientHandle,
        receipt: DetachRevocationReceipt,
    ) -> DetachResult {
        let mut state = lock(&self.inner);
        let attachment_id = client.attachment_id();
        if receipt.attachment_id != *attachment_id
            || !client.belongs_to(
                &self.scope,
                &state.snapshot.thread.thread_id,
                self.authority.host_incarnation(),
            )
        {
            return DetachResult::StaleAttachment {
                request_id: receipt.request_id,
                attachment_id: attachment_id.clone(),
            };
        }
        if let Some(receipt) = client.detached_receipt() {
            return DetachResult::AlreadyDetached { receipt };
        }
        let grant = state
            .subscriptions
            .get(attachment_id)
            .or_else(|| state.retired_subscriptions.get(attachment_id))
            .map(|subscriber| subscriber.grant.clone())
            .unwrap_or_else(|| client.grant().clone());
        if grant_digest(&grant) != receipt.revoked_grant_digest {
            return DetachResult::StaleAttachment {
                request_id: receipt.request_id,
                attachment_id: attachment_id.clone(),
            };
        }
        state.subscriptions.remove(attachment_id);
        state.retired_subscriptions.remove(attachment_id);
        match client.remember_detached(receipt.clone()) {
            Ok(()) => {
                self.notify.notify_all();
                DetachResult::Detached { receipt }
            }
            Err(receipt) => DetachResult::AlreadyDetached { receipt },
        }
    }

    pub fn subscriber_count(&self) -> usize {
        lock(&self.inner).subscriptions.len()
    }

    pub(crate) fn has_live_attachment(&self, attachment_id: &SurfaceAttachmentId) -> bool {
        lock(&self.inner).subscriptions.contains_key(attachment_id)
    }

    #[allow(dead_code)]
    pub(crate) fn admits_client(&self, client: &RuntimeSurfaceClientHandle) -> bool {
        let state = lock(&self.inner);
        let belongs = client.belongs_to(
            &self.scope,
            &state.snapshot.thread.thread_id,
            self.authority.host_incarnation(),
        );
        let live = client.detached_receipt().is_none();
        let grant = state
            .subscriptions
            .get(client.attachment_id())
            .is_some_and(|subscriber| &subscriber.grant == client.grant());
        belongs && live && grant
    }

    pub(crate) fn select_interaction_attachment_for(
        &self,
        kind: SurfaceInteractionKind,
        preferred: Option<&SurfaceAttachmentId>,
    ) -> Option<SurfaceAttachmentId> {
        self.select_interaction_attachment_excluding(kind, preferred, None)
    }

    pub(crate) fn select_interaction_attachment_excluding(
        &self,
        kind: SurfaceInteractionKind,
        preferred: Option<&SurfaceAttachmentId>,
        excluded: Option<&SurfaceAttachmentId>,
    ) -> Option<SurfaceAttachmentId> {
        let state = lock(&self.inner);
        let eligible = |subscriber: &SurfaceSubscriber| {
            subscriber.claimed
                && subscriber
                    .grant
                    .capabilities
                    .as_set()
                    .contains(&SurfaceCapability::RespondGrantedInteraction)
                && subscriber.interaction_kinds.contains(&kind)
        };
        preferred
            .filter(|attachment_id| excluded != Some(*attachment_id))
            .and_then(|attachment_id| {
                state
                    .subscriptions
                    .get(attachment_id)
                    .filter(|subscriber| eligible(subscriber))
                    .map(|_| attachment_id.clone())
            })
            .or_else(|| {
                state
                    .subscriptions
                    .iter()
                    .filter(|(attachment_id, _)| excluded != Some(*attachment_id))
                    .filter(|(_, subscriber)| eligible(subscriber))
                    .min_by_key(|(attachment_id, subscriber)| {
                        (
                            interaction_role_priority(subscriber.grant.role),
                            subscriber.grant.granted_at.next_seq,
                            (*attachment_id).clone(),
                        )
                    })
                    .map(|(attachment_id, _)| attachment_id.clone())
            })
    }

    pub(crate) fn admits_interaction_client(
        &self,
        client: &RuntimeSurfaceClientHandle,
        kind: SurfaceInteractionKind,
    ) -> bool {
        let state = lock(&self.inner);
        client.belongs_to(
            &self.scope,
            &state.snapshot.thread.thread_id,
            self.authority.host_incarnation(),
        ) && client.detached_receipt().is_none()
            && state
                .subscriptions
                .get(client.attachment_id())
                .is_some_and(|subscriber| {
                    &subscriber.grant == client.grant()
                        && subscriber
                            .grant
                            .capabilities
                            .as_set()
                            .contains(&SurfaceCapability::RespondGrantedInteraction)
                        && subscriber.interaction_kinds.contains(&kind)
                })
    }

    pub(crate) fn admits_interaction_attachment(
        &self,
        attachment_id: &SurfaceAttachmentId,
        kind: SurfaceInteractionKind,
    ) -> bool {
        let state = lock(&self.inner);
        state
            .subscriptions
            .get(attachment_id)
            .is_some_and(|subscriber| {
                subscriber.claimed
                    && subscriber
                        .grant
                        .capabilities
                        .as_set()
                        .contains(&SurfaceCapability::RespondGrantedInteraction)
                    && subscriber.interaction_kinds.contains(&kind)
            })
    }

    pub(crate) fn thread_id(&self) -> SurfaceThreadId {
        self.authority.thread_id().clone()
    }

    pub(crate) fn authority(&self) -> &SurfaceAttachAuthority {
        &self.authority
    }

    fn authorize(
        &self,
        role: SurfaceAttachmentRole,
        requested_capabilities: &BTreeSet<SurfaceCapability>,
        requested_interactions: &BTreeSet<SurfaceInteractionKind>,
        granted_at: &SurfaceCursor,
    ) -> Result<SurfaceAttachmentCapabilities, AttachDeniedReason> {
        let authority = &self.authority;
        if role != authority.role() {
            return Err(AttachDeniedReason::RoleMismatch);
        }
        let granted = requested_capabilities
            .intersection(authority.maximum_capabilities().as_set())
            .copied()
            .collect::<BTreeSet<_>>();
        if !authority
            .required_capabilities()
            .as_set()
            .is_subset(&granted)
        {
            return Err(AttachDeniedReason::MissingRequiredCapability);
        }
        let capabilities = NonEmptySet::try_new(granted)
            .map_err(|_| AttachDeniedReason::MissingRequiredCapability)?;
        let attachment_id = next_attachment_id();
        Ok(SurfaceAttachmentCapabilities {
            grant: SurfaceAttachmentGrant {
                attachment_id,
                host_incarnation: authority.host_incarnation().clone(),
                role,
                capabilities,
                granted_at: granted_at.clone(),
                expires_at: None,
            },
            interaction_kinds: requested_interactions
                .intersection(authority.maximum_interaction_kinds())
                .copied()
                .collect(),
            acp_capability_revision: None,
        })
    }

    fn register(
        &self,
        state: &mut SurfaceHubState,
        capabilities: SurfaceAttachmentCapabilities,
        acp_capabilities: Option<AcpAttachmentCapabilityProfile>,
    ) -> (
        SurfaceAttachmentId,
        RuntimeSurfaceClientHandle,
        SurfaceSubscriptionHandle,
        SurfaceAttachmentCapabilities,
    ) {
        let attachment_id = capabilities.grant.attachment_id.clone();
        let client = RuntimeSurfaceClientHandle::new(
            attachment_id.clone(),
            state.snapshot.thread.thread_id.clone(),
            self.authority.host_incarnation().clone(),
            capabilities.grant.clone(),
            self.authority.connection_id().cloned(),
            self.scope.clone(),
        )
        .with_dispatcher(self.dispatcher.clone());
        let hub = Arc::downgrade(&self.inner);
        let subscription =
            SurfaceSubscriptionHandle::new(attachment_id.clone(), move |attachment_id| {
                reclaim_unclaimed_subscription(&hub, attachment_id);
            });
        let (acp_read_dispatch_tx, acp_read_dispatch_rx) =
            if acp_capabilities.is_some_and(|profile| profile.standard.file_read) {
                let (sender, receiver) = sync_channel(1);
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };
        let (acp_write_dispatch_tx, acp_write_dispatch_rx) =
            if acp_capabilities.is_some_and(|profile| profile.standard.file_write) {
                let (sender, receiver) = sync_channel(1);
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };
        let (acp_terminal_create_dispatch_tx, acp_terminal_create_dispatch_rx) =
            if acp_capabilities.is_some_and(|profile| profile.standard.terminal) {
                let (sender, receiver) = sync_channel(1);
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };
        let (acp_terminal_observation_dispatch_tx, acp_terminal_observation_dispatch_rx) =
            if acp_capabilities.is_some_and(|profile| profile.standard.terminal) {
                let (sender, receiver) = sync_channel(1);
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };
        let (acp_terminal_cleanup_dispatch_tx, acp_terminal_cleanup_dispatch_rx) =
            if acp_capabilities.is_some_and(|profile| profile.standard.terminal) {
                let (sender, receiver) = sync_channel(1);
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };
        state.subscriptions.insert(
            attachment_id.clone(),
            SurfaceSubscriber {
                grant: capabilities.grant.clone(),
                interaction_kinds: capabilities.interaction_kinds.clone(),
                acp_capabilities,
                acp_read_dispatch_tx,
                acp_read_dispatch_rx,
                acp_write_dispatch_tx,
                acp_write_dispatch_rx,
                acp_terminal_create_dispatch_tx,
                acp_terminal_create_dispatch_rx,
                acp_terminal_observation_dispatch_tx,
                acp_terminal_observation_dispatch_rx,
                acp_terminal_cleanup_dispatch_tx,
                acp_terminal_cleanup_dispatch_rx,
                queue: VecDeque::new(),
                queued_events: 0,
                queued_bytes: 0,
                claimed: false,
                gapped: false,
            },
        );
        (attachment_id, client, subscription, capabilities)
    }
}

fn interaction_role_priority(role: SurfaceAttachmentRole) -> u8 {
    match role {
        SurfaceAttachmentRole::Tui => 0,
        SurfaceAttachmentRole::Acp => 1,
        SurfaceAttachmentRole::Jsonl => 2,
        SurfaceAttachmentRole::InternalCompatibility => 3,
    }
}

fn acp_standard_capability_supports(
    capabilities: AcpStandardCapabilitySet,
    kind: SurfaceCapabilityCallKind,
) -> bool {
    match kind {
        SurfaceCapabilityCallKind::ReadTextFile => capabilities.file_read,
        SurfaceCapabilityCallKind::WriteTextFile => capabilities.file_write,
        SurfaceCapabilityCallKind::TerminalCreate
        | SurfaceCapabilityCallKind::TerminalOutput
        | SurfaceCapabilityCallKind::TerminalWaitForExit
        | SurfaceCapabilityCallKind::TerminalKill
        | SurfaceCapabilityCallKind::TerminalRelease => capabilities.terminal,
    }
}

fn capture_retained_replay(
    state: &SurfaceHubState,
    supplied: &SurfaceCursor,
) -> Result<Vec<Arc<SharedSurfaceBatch>>, AttachResult> {
    let head = &state.snapshot.cursor;
    if supplied.thread_id != head.thread_id {
        return Err(AttachResult::InvalidCursor {
            error: InvalidCursor {
                reason: InvalidCursorReason::WrongThread,
                supplied: supplied.clone(),
                expected_thread: head.thread_id.clone(),
                head: head.clone(),
            },
        });
    }
    if supplied.incarnation != head.incarnation {
        return Err(snapshot_required(
            state,
            SnapshotRequiredReason::StaleIncarnation,
        ));
    }
    if supplied.next_seq.get() > head.next_seq.get() {
        return Err(invalid_cursor(
            state,
            supplied,
            InvalidCursorReason::FutureSequence,
        ));
    }
    let retained_from = state
        .retained
        .front()
        .map(|batch| &batch.batch.cursor_before)
        .unwrap_or(head);
    if supplied.next_seq.get() < retained_from.next_seq.get() {
        return Err(snapshot_required(
            state,
            if state.replay_hole {
                SnapshotRequiredReason::ReplayHole
            } else {
                SnapshotRequiredReason::ExpiredSuffix
            },
        ));
    }
    let known_boundary = std::iter::once(head)
        .chain(
            state
                .retained
                .iter()
                .map(|batch| &batch.batch.cursor_before),
        )
        .chain(state.retained.iter().map(|batch| &batch.batch.cursor_after))
        .find(|cursor| cursor.next_seq == supplied.next_seq);
    let Some(boundary) = known_boundary else {
        return Err(invalid_cursor(
            state,
            supplied,
            InvalidCursorReason::NotBatchBoundary,
        ));
    };
    if boundary != supplied {
        return Err(invalid_cursor(
            state,
            supplied,
            InvalidCursorReason::ImpossibleSourceRevision,
        ));
    }
    let mut replay = Vec::new();
    let mut expected = supplied.clone();
    for batch in &state.retained {
        if batch.batch.cursor_after.next_seq.get() <= supplied.next_seq.get() {
            continue;
        }
        if batch.batch.cursor_before != expected {
            return Err(snapshot_required(state, SnapshotRequiredReason::ReplayHole));
        }
        replay.push(batch.clone());
        expected = batch.batch.cursor_after.clone();
    }
    if expected != *head {
        return Err(snapshot_required(state, SnapshotRequiredReason::ReplayHole));
    }
    Ok(replay)
}

fn materialize_replay(replay: Vec<Arc<SharedSurfaceBatch>>) -> Vec<SurfaceCommitBatch> {
    replay
        .into_iter()
        .map(|batch| batch.batch.clone())
        .collect()
}

fn dequeue_subscription_item(subscriber: &mut SurfaceSubscriber) -> Option<QueuedSubscriptionItem> {
    let item = subscriber.queue.pop_front()?;
    if let QueuedSubscriptionItem::Batch(batch) = &item {
        subscriber.queued_events = subscriber
            .queued_events
            .saturating_sub(batch.batch.event_count as u64);
        subscriber.queued_bytes = subscriber.queued_bytes.saturating_sub(batch.encoded_bytes);
    }
    Some(item)
}

fn take_subscription_item(
    state: &mut SurfaceHubState,
    attachment_id: &SurfaceAttachmentId,
) -> Option<QueuedSubscriptionItem> {
    let subscriber = subscriber_mut(state, attachment_id)?;
    let item = dequeue_subscription_item(subscriber)?;
    if matches!(item, QueuedSubscriptionItem::Gap(_)) {
        state.retired_subscriptions.remove(attachment_id);
    }
    Some(item)
}

fn materialize_subscription_item(item: QueuedSubscriptionItem) -> SurfaceSubscriptionItem {
    match item {
        QueuedSubscriptionItem::Batch(batch) => SurfaceSubscriptionItem::Batch {
            batch: batch.batch.clone(),
        },
        QueuedSubscriptionItem::Gap(required) => SurfaceSubscriptionItem::Gap { required },
        QueuedSubscriptionItem::Sealed(reason) => SurfaceSubscriptionItem::Sealed { reason },
    }
}

fn invalid_cursor(
    state: &SurfaceHubState,
    supplied: &SurfaceCursor,
    reason: InvalidCursorReason,
) -> AttachResult {
    AttachResult::InvalidCursor {
        error: InvalidCursor {
            reason,
            supplied: supplied.clone(),
            expected_thread: state.snapshot.thread.thread_id.clone(),
            head: state.snapshot.cursor.clone(),
        },
    }
}

fn snapshot_required(state: &SurfaceHubState, reason: SnapshotRequiredReason) -> AttachResult {
    AttachResult::SnapshotRequired {
        required: SnapshotRequired {
            reason,
            retained_from: state
                .retained
                .front()
                .map(|batch| batch.batch.cursor_before.clone()),
            head: state.snapshot.cursor.clone(),
        },
    }
}

fn publish_batch(
    state: &mut SurfaceHubState,
    batch: Arc<SharedSurfaceBatch>,
    config: SurfaceHubConfig,
) -> Vec<SurfaceAttachmentId> {
    let retained_from = state
        .retained
        .front()
        .map(|retained| retained.batch.cursor_before.clone());
    let mut overflowed = Vec::new();
    for (attachment_id, subscriber) in &mut state.subscriptions {
        if subscriber.gapped {
            continue;
        }
        if !enqueue_batch(subscriber, batch.clone(), config) {
            subscriber
                .queue
                .push_back(QueuedSubscriptionItem::Gap(SnapshotRequired {
                    reason: SnapshotRequiredReason::SlowSubscriber,
                    retained_from: retained_from.clone(),
                    head: batch.batch.cursor_after.clone(),
                }));
            subscriber.gapped = true;
            overflowed.push(attachment_id.clone());
        }
    }
    retire_subscriptions(state, &overflowed);
    overflowed
}

fn enqueue_batch(
    subscriber: &mut SurfaceSubscriber,
    batch: Arc<SharedSurfaceBatch>,
    config: SurfaceHubConfig,
) -> bool {
    let exceeds_events = subscriber
        .queued_events
        .checked_add(batch.batch.event_count as u64)
        .is_none_or(|events| events > config.subscriber_event_limit);
    let exceeds_bytes = subscriber
        .queued_bytes
        .checked_add(batch.encoded_bytes)
        .is_none_or(|queued| queued > config.subscriber_byte_limit);
    if exceeds_events || exceeds_bytes {
        return false;
    }
    subscriber.queued_events += batch.batch.event_count as u64;
    subscriber.queued_bytes += batch.encoded_bytes;
    subscriber
        .queue
        .push_back(QueuedSubscriptionItem::Batch(batch));
    true
}

fn retain_batch(
    state: &mut SurfaceHubState,
    batch: Arc<SharedSurfaceBatch>,
    config: SurfaceHubConfig,
) {
    state.retained_events = state
        .retained_events
        .saturating_add(batch.batch.event_count as u64);
    state.retained_bytes = state.retained_bytes.saturating_add(batch.encoded_bytes);
    state.retained.push_back(batch);
    while state.retained_events > config.retained_event_limit
        || state.retained_bytes > config.retained_byte_limit
    {
        let Some(expired) = state.retained.pop_front() else {
            break;
        };
        state.retained_events = state
            .retained_events
            .saturating_sub(expired.batch.event_count as u64);
        state.retained_bytes = state.retained_bytes.saturating_sub(expired.encoded_bytes);
    }
}

fn signal_gap(
    state: &mut SurfaceHubState,
    reason: SnapshotRequiredReason,
    head: SurfaceCursor,
) -> Vec<SurfaceAttachmentId> {
    let retained_from = state
        .retained
        .front()
        .map(|batch| batch.batch.cursor_before.clone());
    let mut gapped = Vec::new();
    for (attachment_id, subscriber) in &mut state.subscriptions {
        subscriber
            .queue
            .push_back(QueuedSubscriptionItem::Gap(SnapshotRequired {
                reason,
                retained_from: retained_from.clone(),
                head: head.clone(),
            }));
        subscriber.gapped = true;
        gapped.push(attachment_id.clone());
    }
    retire_subscriptions(state, &gapped);
    gapped
}

fn retire_subscriptions(state: &mut SurfaceHubState, attachment_ids: &[SurfaceAttachmentId]) {
    for attachment_id in attachment_ids {
        if let Some(subscriber) = state.subscriptions.remove(&attachment_id) {
            state
                .retired_subscriptions
                .insert(attachment_id.clone(), subscriber);
        }
    }
}

fn subscriber_mut<'a>(
    state: &'a mut SurfaceHubState,
    attachment_id: &SurfaceAttachmentId,
) -> Option<&'a mut SurfaceSubscriber> {
    if state.subscriptions.contains_key(attachment_id) {
        state.subscriptions.get_mut(attachment_id)
    } else {
        state.retired_subscriptions.get_mut(attachment_id)
    }
}

fn reclaim_unclaimed_subscription(
    hub: &Weak<Mutex<SurfaceHubState>>,
    attachment_id: &SurfaceAttachmentId,
) {
    let Some(hub) = hub.upgrade() else {
        return;
    };
    let mut state = lock(&hub);
    if state
        .subscriptions
        .get(attachment_id)
        .is_some_and(|subscriber| !subscriber.claimed)
    {
        state.subscriptions.remove(attachment_id);
        return;
    }
    if state
        .retired_subscriptions
        .get(attachment_id)
        .is_some_and(|subscriber| !subscriber.claimed)
    {
        state.retired_subscriptions.remove(attachment_id);
    }
}

fn has_registration_capacity(state: &SurfaceHubState, config: SurfaceHubConfig) -> bool {
    state.subscriptions.len() < config.maximum_subscribers
        && state
            .subscriptions
            .len()
            .saturating_add(state.retired_subscriptions.len())
            < config.maximum_subscribers.saturating_mul(2)
}

fn clear_retained(state: &mut SurfaceHubState) {
    state.retained.clear();
    state.retained_events = 0;
    state.retained_bytes = 0;
}

fn grant_digest(grant: &SurfaceAttachmentGrant) -> Sha256Digest {
    let bytes = serde_json::to_vec(grant).unwrap_or_default();
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    Sha256Digest::new(digest)
}

fn next_attachment_id() -> SurfaceAttachmentId {
    SurfaceAttachmentId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
        .expect("uuid crate returned a valid v7 attachment id")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_surface::commands::{SurfaceEvent, SurfaceEventEnvelope};
    use crate::runtime_surface::identity::{
        CommitClass, CursorSourceRevision, DisplayText, DurableRevision, NonEmptyVec,
        SequenceNumber, SurfaceCommitId, SurfaceConnectionId, SurfaceEventId, SurfaceRequestId,
        SurfaceScope, ThreadOwnerEpoch,
    };
    use crate::runtime_surface::operation::FailureClass;
    use crate::runtime_surface::projection::SessionPatch;
    use crate::runtime_surface::reducer::{
        canonical_batch_digest,
        tests::{reducer_snapshot, uuid_v7_bytes},
    };

    fn subscriber(seed: u8, cursor: &SurfaceCursor) -> SurfaceSubscriber {
        let attachment_id = SurfaceAttachmentId::try_from_bytes(uuid_v7_bytes(seed)).unwrap();
        let host_incarnation = HostIncarnation::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
        let grant = SurfaceAttachmentGrant {
            attachment_id: attachment_id.clone(),
            host_incarnation: host_incarnation.clone(),
            role: SurfaceAttachmentRole::Tui,
            capabilities: NonEmptySet::try_new(BTreeSet::from([SurfaceCapability::ReadSnapshot]))
                .unwrap(),
            granted_at: cursor.clone(),
            expires_at: None,
        };
        SurfaceSubscriber {
            grant: grant.clone(),
            interaction_kinds: BTreeSet::new(),
            acp_capabilities: None,
            acp_read_dispatch_tx: None,
            acp_read_dispatch_rx: None,
            acp_write_dispatch_tx: None,
            acp_write_dispatch_rx: None,
            acp_terminal_create_dispatch_tx: None,
            acp_terminal_create_dispatch_rx: None,
            acp_terminal_observation_dispatch_tx: None,
            acp_terminal_observation_dispatch_rx: None,
            acp_terminal_cleanup_dispatch_tx: None,
            acp_terminal_cleanup_dispatch_rx: None,
            queue: VecDeque::new(),
            queued_events: 0,
            queued_bytes: 0,
            claimed: false,
            gapped: false,
        }
    }

    fn batch() -> SurfaceCommitBatch {
        let snapshot = reducer_snapshot();
        let commit_class = CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(2).unwrap(),
            commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(210)).unwrap(),
        };
        let event = SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(211)).unwrap(),
            commit_class: commit_class.clone(),
            scope: SurfaceScope::Thread,
            event: SurfaceEvent::Session(SessionPatch::RuntimeFault {
                class: FailureClass::Persistence,
                message: DisplayText::new("shared"),
                causative_generation: None,
            }),
        };
        let mut batch = SurfaceCommitBatch {
            cursor_before: snapshot.cursor.clone(),
            cursor_after: SurfaceCursor {
                next_seq: SequenceNumber::new(snapshot.cursor.next_seq.get() + 1),
                source_revision: CursorSourceRevision::Recorded {
                    durable_revision: DurableRevision::try_new(2).unwrap(),
                },
                ..snapshot.cursor
            },
            commit_class,
            event_count: 1,
            batch_digest: Sha256Digest::new([0; 32]),
            events: NonEmptyVec::try_new(vec![event]).unwrap(),
        };
        batch.batch_digest = canonical_batch_digest(&batch);
        batch
    }

    #[test]
    fn subscriber_lanes_share_one_cached_batch_allocation() {
        let batch = Arc::new(SharedSurfaceBatch::new(batch()));
        let cursor = batch.batch.cursor_before.clone();
        let mut first = subscriber(212, &cursor);
        let mut second = subscriber(214, &cursor);
        let config = SurfaceHubConfig::default();

        assert!(enqueue_batch(&mut first, batch.clone(), config));
        assert!(enqueue_batch(&mut second, batch.clone(), config));

        let QueuedSubscriptionItem::Batch(first_batch) = first.queue.front().unwrap() else {
            panic!("first lane did not receive a batch");
        };
        let QueuedSubscriptionItem::Batch(second_batch) = second.queue.front().unwrap() else {
            panic!("second lane did not receive a batch");
        };
        assert!(Arc::ptr_eq(first_batch, second_batch));
        assert!(Arc::ptr_eq(first_batch, &batch));
    }

    #[test]
    fn locked_paths_capture_shared_batches_before_public_materialization() {
        let batch = Arc::new(SharedSurfaceBatch::new(batch()));
        let cursor_before = batch.batch.cursor_before.clone();
        let mut subscriber = subscriber(216, &cursor_before);
        assert!(enqueue_batch(
            &mut subscriber,
            batch.clone(),
            SurfaceHubConfig::default(),
        ));

        let queued = dequeue_subscription_item(&mut subscriber).unwrap();
        let queued_batch = match &queued {
            QueuedSubscriptionItem::Batch(queued_batch) => queued_batch,
            QueuedSubscriptionItem::Gap(_) => {
                panic!("locked dequeue did not return the shared batch")
            }
            QueuedSubscriptionItem::Sealed(_) => {
                panic!("locked dequeue did not return the shared batch")
            }
        };
        assert!(Arc::ptr_eq(&queued_batch, &batch));
        let public = materialize_subscription_item(queued);
        assert!(matches!(
            public,
            SurfaceSubscriptionItem::Batch { batch: owned } if owned == batch.batch
        ));

        let mut snapshot = reducer_snapshot();
        snapshot.cursor = batch.batch.cursor_after.clone();
        let state = SurfaceHubState {
            ready: true,
            seal_reason: None,
            snapshot: Arc::new(snapshot),
            retained: VecDeque::from([batch.clone()]),
            retained_events: batch.batch.event_count as u64,
            retained_bytes: batch.encoded_bytes,
            replay_hole: false,
            subscriptions: BTreeMap::new(),
            retired_subscriptions: BTreeMap::new(),
        };
        let captured = match capture_retained_replay(&state, &cursor_before) {
            Ok(captured) => captured,
            Err(_) => panic!("shared replay capture failed"),
        };
        assert_eq!(captured.len(), 1);
        assert!(Arc::ptr_eq(&captured[0], &batch));
        assert!(materialize_replay(captured) == vec![batch.batch.clone()]);
    }

    #[test]
    fn sealed_subscription_retains_runtime_snapshot_for_terminal_reconciliation() {
        let snapshot = reducer_snapshot();
        let host_incarnation = HostIncarnation::try_from_bytes(uuid_v7_bytes(220)).unwrap();
        let hub = SurfaceHub::new_tui(
            snapshot.clone(),
            host_incarnation,
            SurfaceHubConfig::default(),
        )
        .unwrap();
        hub.repair_committed(Arc::new(snapshot.clone()), &[]);
        let attachment = match hub.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => panic!("fresh attachment"),
        };
        let mut receiver = hub
            .claim_subscription(&attachment.subscription)
            .expect("subscription receiver");

        hub.seal_subscriptions(SurfaceSubscriptionSealReason::HostShutdown);

        assert!(matches!(
            receiver.try_recv(),
            Some(SurfaceSubscriptionItem::Sealed {
                reason: SurfaceSubscriptionSealReason::HostShutdown,
            })
        ));
        let retained = receiver
            .sealed_snapshot()
            .expect("sealed receiver retains runtime snapshot");
        assert_eq!(retained.cursor, snapshot.cursor);
        assert_eq!(
            retained.snapshot.thread.thread_id,
            snapshot.thread.thread_id
        );
    }

    #[test]
    fn subscription_recv_timeout_wakes_on_a_published_batch() {
        let initial = reducer_snapshot();
        let hub = SurfaceHub::new_tui(
            initial.clone(),
            HostIncarnation::try_from_bytes(uuid_v7_bytes(218)).unwrap(),
            SurfaceHubConfig::default(),
        )
        .unwrap();
        hub.repair_committed(Arc::new(initial.clone()), &[]);
        let attachment = match hub.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::try_from_bytes(uuid_v7_bytes(219)).unwrap(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => panic!("fresh attach failed"),
        };
        let mut receiver = hub.claim_subscription(&attachment.subscription).unwrap();
        let batch = batch();
        let mut next = initial;
        next.cursor = batch.cursor_after.clone();
        let publish_hub = hub.clone();
        let publisher = std::thread::spawn(move || {
            publish_hub.apply_committed(Arc::new(next), &batch);
        });

        assert!(matches!(
            receiver.recv_timeout(std::time::Duration::from_secs(1)),
            Some(SurfaceSubscriptionItem::Batch { .. })
        ));
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(1))
                .is_none()
        );
        publisher.join().unwrap();
    }

    #[test]
    fn live_client_admission_requires_exact_active_hub_grant() {
        let snapshot = reducer_snapshot();
        let capabilities =
            NonEmptySet::try_new(BTreeSet::from([SurfaceCapability::ReadSnapshot])).unwrap();
        let make_hub = || {
            let host_incarnation = HostIncarnation::try_from_bytes(uuid_v7_bytes(220)).unwrap();
            let authority = SurfaceAttachAuthority::new(
                host_incarnation.clone(),
                snapshot.thread.thread_id.clone(),
                SurfaceAttachmentRole::Tui,
                capabilities.clone(),
                capabilities.clone(),
                BTreeSet::new(),
            );
            SurfaceHub::from_authority(snapshot.clone(), authority, SurfaceHubConfig::default())
                .unwrap()
        };
        let hub = make_hub();
        hub.repair_committed(Arc::new(snapshot.clone()), &[]);
        let attachment = match hub.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::try_from_bytes(uuid_v7_bytes(221)).unwrap(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => panic!("fresh attach failed"),
        };
        let foreign = make_hub();
        foreign.repair_committed(Arc::new(snapshot), &[]);

        assert!(hub.admits_client(&attachment.client));
        assert!(!foreign.admits_client(&attachment.client));
        assert!(matches!(
            hub.detach(
                &attachment.client,
                DetachRequest {
                    request_id: SurfaceRequestId::try_from_bytes(uuid_v7_bytes(222)).unwrap(),
                },
            ),
            DetachResult::Detached { .. }
        ));
        assert!(!hub.admits_client(&attachment.client));
    }

    #[test]
    fn interaction_routing_requires_a_claimed_live_subscription() {
        let snapshot = reducer_snapshot();
        let capabilities = NonEmptySet::try_new(BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::RespondGrantedInteraction,
        ]))
        .unwrap();
        let authority = SurfaceAttachAuthority::new(
            HostIncarnation::try_from_bytes(uuid_v7_bytes(223)).unwrap(),
            snapshot.thread.thread_id.clone(),
            SurfaceAttachmentRole::Tui,
            capabilities.clone(),
            NonEmptySet::try_new(BTreeSet::from([SurfaceCapability::ReadSnapshot])).unwrap(),
            BTreeSet::from([SurfaceInteractionKind::UserInput]),
        );
        let hub =
            SurfaceHub::from_authority(snapshot.clone(), authority, Default::default()).unwrap();
        hub.repair_committed(Arc::new(snapshot), &[]);
        let attachment = match hub.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::try_from_bytes(uuid_v7_bytes(224)).unwrap(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: capabilities.as_set().clone(),
            interaction_capabilities: BTreeSet::from([SurfaceInteractionKind::UserInput]),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => panic!("fresh interaction attach failed"),
        };

        assert_eq!(
            hub.select_interaction_attachment_for(SurfaceInteractionKind::UserInput, None),
            None
        );
        let _receiver = hub
            .claim_subscription(&attachment.subscription)
            .expect("claim subscription once");
        assert_eq!(
            hub.select_interaction_attachment_for(
                SurfaceInteractionKind::UserInput,
                Some(&attachment.attachment_id),
            ),
            Some(attachment.attachment_id)
        );
    }

    #[test]
    fn interaction_routing_fallback_uses_grant_order_before_attachment_id() {
        let snapshot = reducer_snapshot();
        let capabilities = NonEmptySet::try_new(BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::RespondGrantedInteraction,
        ]))
        .unwrap();
        let authority = SurfaceAttachAuthority::new(
            HostIncarnation::try_from_bytes(uuid_v7_bytes(230)).unwrap(),
            snapshot.thread.thread_id.clone(),
            SurfaceAttachmentRole::Tui,
            capabilities.clone(),
            NonEmptySet::try_new(BTreeSet::from([SurfaceCapability::ReadSnapshot])).unwrap(),
            BTreeSet::from([SurfaceInteractionKind::UserInput]),
        );
        let hub =
            SurfaceHub::from_authority(snapshot.clone(), authority, Default::default()).unwrap();
        hub.repair_committed(Arc::new(snapshot.clone()), &[]);
        let attach = || match hub.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: capabilities.as_set().clone(),
            interaction_capabilities: BTreeSet::from([SurfaceInteractionKind::UserInput]),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => panic!("fresh interaction attach failed"),
        };
        let lower_id = attach();
        let higher_id = attach();
        let _lower_receiver = hub.claim_subscription(&lower_id.subscription).unwrap();
        let _higher_receiver = hub.claim_subscription(&higher_id.subscription).unwrap();
        let (lower_id, higher_id) = if lower_id.attachment_id < higher_id.attachment_id {
            (lower_id.attachment_id, higher_id.attachment_id)
        } else {
            (higher_id.attachment_id, lower_id.attachment_id)
        };
        {
            let mut state = lock(&hub.inner);
            state
                .subscriptions
                .get_mut(&lower_id)
                .unwrap()
                .grant
                .granted_at
                .next_seq = SequenceNumber::new(10);
            state
                .subscriptions
                .get_mut(&higher_id)
                .unwrap()
                .grant
                .granted_at
                .next_seq = SequenceNumber::new(1);
        }

        assert_eq!(
            hub.select_interaction_attachment_for(SurfaceInteractionKind::UserInput, None),
            Some(higher_id),
            "earlier grant order wins even when its attachment id sorts later"
        );
    }

    #[test]
    fn acp_capability_routes_require_claimed_live_revisioned_attachment() {
        let snapshot = reducer_snapshot();
        let capabilities =
            NonEmptySet::try_new(BTreeSet::from([SurfaceCapability::ReadSnapshot])).unwrap();
        let authority = SurfaceAttachAuthority::new(
            HostIncarnation::try_from_bytes(uuid_v7_bytes(240)).unwrap(),
            snapshot.thread.thread_id.clone(),
            SurfaceAttachmentRole::Acp,
            capabilities.clone(),
            capabilities.clone(),
            BTreeSet::new(),
        )
        .with_connection_id(SurfaceConnectionId::try_from_bytes(uuid_v7_bytes(241)).unwrap());
        let hub =
            SurfaceHub::from_authority(snapshot.clone(), authority, Default::default()).unwrap();
        hub.repair_committed(Arc::new(snapshot.clone()), &[]);
        let attach = |capability_profile| match hub.attach_acp_fresh(
            FreshAttachRequest {
                request_id: SurfaceRequestId::new(),
                role: SurfaceAttachmentRole::Acp,
                requested_capabilities: capabilities.as_set().clone(),
                interaction_capabilities: BTreeSet::new(),
            },
            capability_profile,
        ) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => panic!("fresh ACP capability attachment failed"),
        };

        let read_attachment = attach(AcpAttachmentCapabilityProfile {
            revision: CapabilityRevision::try_new(7).unwrap(),
            standard: AcpStandardCapabilitySet {
                file_read: true,
                ..AcpStandardCapabilitySet::default()
            },
        });
        assert_eq!(
            read_attachment.capabilities.acp_capability_revision,
            Some(CapabilityRevision::try_new(7).unwrap())
        );
        assert!(
            hub.select_acp_capability_attachment(
                SurfaceCapabilityCallKind::ReadTextFile,
                &read_attachment.attachment_id,
            )
            .is_none(),
            "an unclaimed transport cannot receive runtime calls"
        );
        let read_receiver = hub
            .claim_subscription(&read_attachment.subscription)
            .expect("claim read-capable ACP attachment");
        let read_dispatch_receiver = hub
            .claim_acp_read_text_file_dispatch(&read_attachment.client)
            .expect("claim read capability transport lane");
        let route = hub
            .select_acp_capability_attachment(
                SurfaceCapabilityCallKind::ReadTextFile,
                &read_attachment.attachment_id,
            )
            .expect("claimed read capability route");
        assert_eq!(route.attachment_id, read_attachment.attachment_id);
        assert_eq!(
            route.capability_revision,
            CapabilityRevision::try_new(7).unwrap()
        );
        let dispatch = || AcpReadTextFileDispatch {
            call_id: SurfaceCapabilityCallId::try_from_bytes(uuid_v7_bytes(242)).unwrap(),
            acp_session_id: NonEmptyText::try_new("session-1").unwrap(),
            capability_revision: route.capability_revision,
            path: CanonicalPath::try_new(std::env::temp_dir().join("orca-acp-read.txt")).unwrap(),
            line: Some(3),
            limit: Some(7),
        };
        hub.dispatch_acp_read_text_file(&route, dispatch())
            .expect("first read dispatch enters the bounded lane");
        assert_eq!(
            hub.dispatch_acp_read_text_file(&route, dispatch()),
            Err(AcpCapabilityDispatchError::Full),
            "the runtime never creates an unbounded ACP capability queue"
        );
        assert_eq!(
            read_dispatch_receiver.try_recv(),
            Ok(dispatch()),
            "the exact revisioned dispatch reaches its origin attachment"
        );
        assert!(
            hub.select_acp_capability_attachment(
                SurfaceCapabilityCallKind::WriteTextFile,
                &read_attachment.attachment_id,
            )
            .is_none()
        );

        let other_read_attachment = attach(AcpAttachmentCapabilityProfile {
            revision: CapabilityRevision::try_new(8).unwrap(),
            standard: AcpStandardCapabilitySet {
                file_read: true,
                ..AcpStandardCapabilitySet::default()
            },
        });
        let other_read_receiver = hub
            .claim_subscription(&other_read_attachment.subscription)
            .expect("claim newer read-capable ACP attachment");
        assert_eq!(
            hub.select_acp_capability_attachment(
                SurfaceCapabilityCallKind::ReadTextFile,
                &read_attachment.attachment_id,
            )
            .unwrap()
            .attachment_id,
            read_attachment.attachment_id,
            "operation-bound routing preserves its origin attachment"
        );

        drop(read_receiver);
        assert!(
            hub.select_acp_capability_attachment(
                SurfaceCapabilityCallKind::ReadTextFile,
                &read_attachment.attachment_id,
            )
            .is_none(),
            "dropping the origin transport must not fall back to another client"
        );
        assert_eq!(
            hub.dispatch_acp_read_text_file(&route, dispatch()),
            Err(AcpCapabilityDispatchError::StaleRoute),
            "a detached origin cannot retain capability dispatch authority"
        );
        drop(other_read_receiver);

        let terminal_attachment = attach(AcpAttachmentCapabilityProfile {
            revision: CapabilityRevision::try_new(9).unwrap(),
            standard: AcpStandardCapabilitySet {
                terminal: true,
                ..AcpStandardCapabilitySet::default()
            },
        });
        assert_eq!(
            terminal_attachment.capabilities.acp_capability_revision,
            Some(CapabilityRevision::try_new(9).unwrap())
        );
        let _terminal_receiver = hub
            .claim_subscription(&terminal_attachment.subscription)
            .expect("claim terminal-capable ACP attachment");
        assert!(
            hub.select_acp_capability_attachment(
                SurfaceCapabilityCallKind::ReadTextFile,
                &terminal_attachment.attachment_id,
            )
            .is_none(),
            "reconnect must not retain the previous capability profile"
        );
        let terminal_route = hub
            .select_acp_capability_attachment(
                SurfaceCapabilityCallKind::TerminalCreate,
                &terminal_attachment.attachment_id,
            )
            .expect("claimed terminal route");
        assert_eq!(
            terminal_route.capability_revision,
            CapabilityRevision::try_new(9).unwrap()
        );

        hub.seal_subscriptions(SurfaceSubscriptionSealReason::HostShutdown);
        assert!(
            hub.select_acp_capability_attachment(
                SurfaceCapabilityCallKind::ReadTextFile,
                &other_read_attachment.attachment_id,
            )
            .is_none(),
            "a sealed hub cannot select a capability route"
        );
        assert!(
            hub.claim_acp_read_text_file_dispatch(&other_read_attachment.client)
                .is_none(),
            "a sealed hub cannot transfer a capability transport lane"
        );
        let sealed_route = AcpCapabilityAttachmentRoute {
            attachment_id: other_read_attachment.attachment_id.clone(),
            capability_revision: CapabilityRevision::try_new(8).unwrap(),
        };
        assert_eq!(
            hub.dispatch_acp_read_text_file(
                &sealed_route,
                AcpReadTextFileDispatch {
                    call_id: SurfaceCapabilityCallId::try_from_bytes(uuid_v7_bytes(243)).unwrap(),
                    acp_session_id: NonEmptyText::try_new("session-1").unwrap(),
                    capability_revision: CapabilityRevision::try_new(8).unwrap(),
                    path: CanonicalPath::try_new(
                        std::env::temp_dir().join("orca-acp-read-after-seal.txt"),
                    )
                    .unwrap(),
                    line: None,
                    limit: None,
                },
            ),
            Err(AcpCapabilityDispatchError::StaleRoute),
            "a sealed hub cannot receive a capability dispatch"
        );
    }

    #[test]
    fn acp_dispatch_retries_a_full_lane_until_the_client_drains() {
        // The captured ACP stall: two back-to-back dispatches overflow the
        // capacity-1 lane between the client's 100ms drain polls, the
        // second frame is lost, and the client waits forever. The retry
        // must deliver the second dispatch once the client drains.
        let (sender, receiver) = sync_channel::<u8>(1);
        sender.send(1).expect("first dispatch fills the lane");
        let sender_clone = sender.clone();
        let send_thread =
            std::thread::spawn(move || send_acp_dispatch_retrying_full(&sender_clone, 2));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(receiver.recv(), Ok(1), "first dispatch drains");
        assert_eq!(
            send_thread.join().expect("dispatch thread joins"),
            Ok(()),
            "the retry delivers the second dispatch after the drain"
        );
        assert_eq!(receiver.recv(), Ok(2), "no frame was lost");
    }
}
