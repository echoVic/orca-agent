use crossbeam_channel as mpsc;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use orca_core::config::RunConfig;
use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};
use orca_runtime::runtime_host::HostedTurnRequest;
use orca_runtime::surface::{
    AttachResult, BackgroundTarget, DisplayText, ExpectedGoal, FreshAttachRequest,
    GoalMutationAction, GoalRunInput, GoalTokenBudgetUpdate, MutationCommitAck, MutationReply,
    NonEmptyText, NonEmptyVec, OperationIngressCorrelation, OperationKind, OperationPatch,
    OperationRequestIntent, OperationSettingsPreparation, OperationTerminal,
    OptionalProcessLocalCancel, PinnedContextAction, PinnedContextSourceRevision,
    PinnedUserRevision, ReplayabilityRequest, RuntimeSettingsPatch, RuntimeSurfaceClientHandle,
    RuntimeSurfaceHandle, RuntimeSurfaceThreadHandle, SessionMetadataPatch,
    SessionMetadataPrecondition, SessionMetadataRevision, Sha256Digest, StaleMutationError,
    SurfaceAllowDeny, SurfaceAttachmentRole, SurfaceCapability, SurfaceCatalogEntryId,
    SurfaceClientInteractionAnswer, SurfaceCursor, SurfaceEvent, SurfaceFactFamily, SurfaceGoal,
    SurfaceGoalFence, SurfaceInputRequest, SurfaceInputRequestBlock, SurfaceInteractionKind,
    SurfaceOperationId, SurfacePinnedContextEntry, SurfacePinnedContextKind, SurfaceRequestId,
    SurfaceSettingsSnapshot, SurfaceSnapshot, SurfaceSubscriptionItem, SurfaceTaskFence,
    SurfaceUnavailableReason, SurfaceWorkflowRunId, TaskControlAction, TransferBackgroundOutput,
    UncommittedMutation, WaitOperationTerminalResult, WorkflowCatalogRevision,
    WorkflowControlAction, WorkflowPatch,
};

use crate::hosted_runtime::TuiHostedOperationOutcome;
use crate::operation_controller::{SurfacePresentationCancellation, TuiSurfaceTaskControl};
use crate::surface_projection::{
    GoalProjectionPresentation, SurfaceProjectionState, TuiSurfaceProjection,
};
use crate::types::TuiEvent;

#[derive(Debug)]
pub(crate) struct TerminalRecoveryRequired(&'static str);

#[cfg(test)]
pub(crate) fn terminal_recovery_error_for_test(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, TerminalRecoveryRequired(message))
}

impl fmt::Display for TerminalRecoveryRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for TerminalRecoveryRequired {}

#[derive(Debug)]
pub(crate) enum SessionMetadataUpdateError {
    Stale { error: StaleMutationError },
    Other(io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TuiSessionMetadataCommit {
    pub(crate) metadata_revision: SessionMetadataRevision,
    pub(crate) thread_cursor: SurfaceCursor,
}

impl SessionMetadataUpdateError {
    pub(crate) fn is_stale(&self) -> bool {
        matches!(self, Self::Stale { .. })
    }
}

impl fmt::Display for SessionMetadataUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale { error } => formatter.write_str(error.error().message.as_str()),
            Self::Other(error) => error.fmt(formatter),
        }
    }
}

impl Error for SessionMetadataUpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Stale { .. } => None,
            Self::Other(error) => Some(error),
        }
    }
}

impl From<io::Error> for SessionMetadataUpdateError {
    fn from(error: io::Error) -> Self {
        Self::Other(error)
    }
}

pub(crate) fn is_terminal_recovery_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<TerminalRecoveryRequired>())
        .is_some()
}

struct SurfaceRunGuard<'a> {
    surface: &'a RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    controller: &'a TuiSurfaceTaskControl,
    operation_id: Option<SurfaceOperationId>,
    controller_installed: bool,
    cancel_on_drop: bool,
}

struct SurfaceActivationGuard<'a> {
    controller: &'a TuiSurfaceTaskControl,
    pending: bool,
}

impl<'a> SurfaceActivationGuard<'a> {
    fn begin(controller: &'a TuiSurfaceTaskControl) -> io::Result<Self> {
        controller.begin_surface_activation()?;
        Ok(Self {
            controller,
            pending: true,
        })
    }

    fn disarm(&mut self) {
        self.pending = false;
    }
}

impl Drop for SurfaceActivationGuard<'_> {
    fn drop(&mut self) {
        if self.pending {
            self.controller.cancel_surface_activation();
        }
    }
}

impl<'a> SurfaceRunGuard<'a> {
    fn new(
        surface: &'a RuntimeSurfaceHandle,
        client: RuntimeSurfaceClientHandle,
        controller: &'a TuiSurfaceTaskControl,
    ) -> Self {
        Self {
            surface,
            client,
            controller,
            operation_id: None,
            controller_installed: false,
            cancel_on_drop: true,
        }
    }

    fn bind_operation(&mut self, operation_id: SurfaceOperationId) {
        self.operation_id = Some(operation_id);
    }

    fn controller_installed(&mut self) {
        self.controller_installed = true;
    }

    fn terminal_observed(&mut self) {
        self.cancel_on_drop = false;
    }

    fn preserve_operation(&mut self) {
        self.cancel_on_drop = false;
    }

    fn operation_started(&mut self) {
        self.cancel_on_drop = true;
    }
}

impl Drop for SurfaceRunGuard<'_> {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            if let Some(operation_id) = self.operation_id.as_ref() {
                let _ = self
                    .client
                    .cancel_operation(SurfaceRequestId::new(), operation_id.clone());
            }
        }
        if self.controller_installed {
            if let Some(operation_id) = self.operation_id.as_ref() {
                self.controller.complete_surface(operation_id);
            }
        }
        detach(self.surface, &self.client);
    }
}

pub(crate) fn run(
    thread: &RuntimeSurfaceThreadHandle,
    request: HostedTurnRequest,
    config: RunConfig,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    run_typed_thread(thread, request, config, controller, event_tx)
}

pub(crate) fn resume_recovered_operation(
    thread: &RuntimeSurfaceThreadHandle,
    operation_id: &SurfaceOperationId,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    control_recovered_operation(
        thread,
        operation_id,
        controller,
        event_tx,
        RecoveryControl::Resume,
    )
}

pub(crate) fn cancel_recovered_operation(
    thread: &RuntimeSurfaceThreadHandle,
    operation_id: &SurfaceOperationId,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    control_recovered_operation(
        thread,
        operation_id,
        controller,
        event_tx,
        RecoveryControl::Cancel,
    )
}

pub(crate) fn manual_compact(
    thread: &RuntimeSurfaceThreadHandle,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    let mut activation = SurfaceActivationGuard::begin(controller)?;
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. } => {
            return Err(io::Error::other(
                "typed TUI manual compaction attachment denied",
            ));
        }
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            return Err(io::Error::other(
                "typed TUI manual compaction surface unavailable",
            ));
        }
    };
    let mut guard = SurfaceRunGuard::new(&surface, attachment.client.clone(), controller);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| io::Error::other("typed TUI surface subscription unavailable"))?;
    let mut projection = TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
    for event in projection.hydrate_open_streams() {
        let _ = event_tx.send(event);
    }
    let output = match attachment
        .client
        .manual_compact(
            SurfaceRequestId::new(),
            attachment.baseline.snapshot.context.revision,
        )
        .map_err(|error| {
            io::Error::other(format!("typed TUI manual compaction failed: {error:?}"))
        })? {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred { mutation, .. } => {
            return Err(io::Error::other(format!(
                "typed TUI manual compaction deferred: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )));
        }
        MutationReply::Uncommitted { mutation } => {
            return Err(io::Error::other(format!(
                "typed TUI manual compaction did not commit: {mutation:?}"
            )));
        }
    };
    let operation_id = output.operation_id;
    guard.bind_operation(operation_id.clone());
    projection.focus_operation(operation_id.clone());
    controller.install_surface(attachment.client.clone(), operation_id.clone())?;
    activation.disarm();
    guard.controller_installed();
    let result = drain_operation_with_boundary(
        &surface,
        &attachment.client,
        &operation_id,
        &mut subscription,
        &mut projection,
        controller,
        event_tx,
        true,
    );
    if result.is_ok() {
        guard.terminal_observed();
    }
    manual_compaction_terminal_outcome(result?)
}

fn manual_compaction_terminal_outcome(
    outcome: TuiHostedOperationOutcome,
) -> io::Result<TuiHostedOperationOutcome> {
    match outcome {
        TuiHostedOperationOutcome::Turn { status }
            if status == "success" || status == "cancelled" =>
        {
            Ok(TuiHostedOperationOutcome::ManualCompaction)
        }
        TuiHostedOperationOutcome::Turn { status } => Err(io::Error::other(format!(
            "typed TUI manual compaction terminated with status {status}"
        ))),
        TuiHostedOperationOutcome::ManualCompaction => {
            Ok(TuiHostedOperationOutcome::ManualCompaction)
        }
    }
}

pub(crate) fn update_settings(
    thread: &RuntimeSurfaceThreadHandle,
    patches: NonEmptyVec<RuntimeSettingsPatch>,
) -> io::Result<SurfaceSettingsSnapshot> {
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManageThreadSettings,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => {
            return Err(io::Error::other(
                "typed TUI settings attachment unavailable",
            ));
        }
    };
    let expected_revision = attachment.baseline.snapshot.settings.thread_revision;
    let result =
        attachment
            .client
            .update_settings(SurfaceRequestId::new(), expected_revision, patches);
    detach(&surface, &attachment.client);
    let result = result.map_err(|error| {
        io::Error::other(format!("typed TUI settings update failed: {error:?}"))
    })?;
    match result {
        MutationReply::Committed { value, .. } => Ok(value.settings),
        MutationReply::Uncommitted { mutation } => Err(io::Error::other(format!(
            "typed TUI settings update did not commit: {mutation:?}"
        ))),
        MutationReply::Deferred { .. } => {
            Err(io::Error::other("typed TUI settings update deferred"))
        }
    }
}

pub(crate) fn read_snapshot(thread: &RuntimeSurfaceThreadHandle) -> io::Result<SurfaceSnapshot> {
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { reason } => {
            return Err(io::Error::other(format!(
                "typed TUI snapshot attachment denied: {reason:?}"
            )));
        }
        AttachResult::Unavailable { reason } => {
            return Err(io::Error::other(format!(
                "typed TUI snapshot attachment unavailable: {reason:?}"
            )));
        }
        AttachResult::ThreadClosed { .. } => {
            return Err(io::Error::other("typed TUI snapshot thread is closed"));
        }
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. } => {
            return Err(io::Error::other(
                "typed TUI snapshot attachment returned an invalid fresh-attach result",
            ));
        }
    };
    let snapshot = (*attachment.baseline.snapshot).clone();
    detach(&surface, &attachment.client);
    Ok(snapshot)
}

pub(crate) fn rebind_background_presentations(
    thread: &RuntimeSurfaceThreadHandle,
    controller: &TuiSurfaceTaskControl,
    event_tx: mpsc::Sender<TuiEvent>,
) -> io::Result<()> {
    let snapshot = read_snapshot(thread)?;
    let surface = thread.surface();
    for background in snapshot.background_operations {
        spawn_background_presentation(
            &surface,
            background.operation_id,
            controller,
            event_tx.clone(),
        )?;
    }
    Ok(())
}

pub(crate) fn update_session_metadata(
    thread: &RuntimeSurfaceThreadHandle,
    expected_revision: SessionMetadataRevision,
    patch: SessionMetadataPatch,
) -> Result<TuiSessionMetadataCommit, SessionMetadataUpdateError> {
    let committed_revision = SessionMetadataRevision::try_new(
        expected_revision
            .get()
            .checked_add(1)
            .ok_or_else(|| io::Error::other("session metadata revision overflow"))?,
    )
    .map_err(|error| io::Error::other(format!("invalid session metadata revision: {error}")))?;
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManageThreadSettings,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { reason } => {
            return Err(io::Error::other(format!(
                "typed TUI metadata attachment denied: {reason:?}"
            ))
            .into());
        }
        AttachResult::Unavailable { reason } => {
            return Err(io::Error::other(format!(
                "typed TUI metadata attachment unavailable: {reason:?}"
            ))
            .into());
        }
        AttachResult::ThreadClosed { .. } => {
            return Err(io::Error::other("typed TUI metadata thread is closed").into());
        }
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. } => {
            return Err(io::Error::other(
                "typed TUI metadata attachment returned an invalid fresh-attach result",
            )
            .into());
        }
    };
    let result = attachment.client.update_session_metadata(
        SurfaceRequestId::new(),
        SessionMetadataPrecondition::Exact {
            revision: expected_revision,
        },
        patch,
    );
    detach(&surface, &attachment.client);
    let result = result.map_err(|error| {
        io::Error::other(format!("typed TUI metadata update failed: {error:?}"))
    })?;
    match result {
        MutationReply::Committed { mutation, .. } => {
            let thread_cursor = mutation
                .acknowledgements
                .as_slice()
                .iter()
                .find_map(|acknowledgement| match acknowledgement {
                    MutationCommitAck::ThreadLocalCursor { cursor, family, .. }
                        if *family == SurfaceFactFamily::Session =>
                    {
                        Some(cursor.clone())
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    io::Error::other(
                        "typed TUI metadata commit returned no session cursor acknowledgement",
                    )
                })?;
            Ok(TuiSessionMetadataCommit {
                metadata_revision: committed_revision,
                thread_cursor,
            })
        }
        MutationReply::Uncommitted {
            mutation: UncommittedMutation::Stale { error, .. },
        } => Err(SessionMetadataUpdateError::Stale { error }),
        MutationReply::Uncommitted { mutation } => Err(io::Error::other(format!(
            "typed TUI metadata update did not commit: {mutation:?}"
        ))
        .into()),
        MutationReply::Deferred { .. } => {
            Err(io::Error::other("typed TUI metadata update deferred").into())
        }
    }
}

pub(crate) fn stop_task(
    thread: &RuntimeSurfaceThreadHandle,
    task_id: &str,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<SurfaceProjectionState, String> {
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManageTask,
            SurfaceCapability::ManageWorkflow,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { reason } => {
            return Err(format!("typed TUI task-stop attachment denied: {reason:?}"));
        }
        AttachResult::Unavailable { reason } => {
            return Err(format!(
                "typed TUI task-stop attachment unavailable: {reason:?}"
            ));
        }
        AttachResult::ThreadClosed { .. } => {
            return Err("typed TUI task-stop thread is closed".to_string());
        }
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. } => {
            return Err(
                "typed TUI task-stop attachment returned an invalid fresh-attach result"
                    .to_string(),
            );
        }
    };
    let Some(task) = attachment
        .baseline
        .snapshot
        .tasks
        .iter()
        .find(|task| task.task_id.as_str() == task_id)
    else {
        detach(&surface, &attachment.client);
        return Err(format!("surface task '{task_id}' not found"));
    };
    if task.task_type == orca_runtime::surface::SurfaceTaskType::MainSession {
        let mut activation =
            SurfaceActivationGuard::begin(controller).map_err(|error| error.to_string())?;
        let operation_id = task
            .parent_operation
            .clone()
            .ok_or_else(|| format!("surface task '{task_id}' has no owning operation"))?;
        let mut guard = SurfaceRunGuard::new(&surface, attachment.client.clone(), controller);
        guard.bind_operation(operation_id.clone());
        guard.preserve_operation();
        let mut subscription = surface
            .claim_subscription(&attachment.subscription)
            .ok_or_else(|| "typed TUI task-stop subscription unavailable".to_string())?;
        let mut projection =
            TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
        projection.focus_operation(operation_id.clone());
        let result = attachment.client.task_control(
            SurfaceRequestId::new(),
            TaskControlAction::Stop {
                fence: SurfaceTaskFence {
                    task_id: task.task_id.clone(),
                    task_revision: task.revision,
                    background_owner: task.background_fence.clone(),
                },
            },
        );
        match result.map_err(|error| format!("typed TUI task stop failed: {error:?}"))? {
            MutationReply::Committed { .. } => {}
            MutationReply::Deferred { mutation, .. } => {
                return Err(format!(
                    "typed TUI task stop deferred: request={:?} commit={:?}",
                    mutation.request_id, mutation.commit_id
                ));
            }
            MutationReply::Uncommitted { mutation } => {
                return Err(format!("typed TUI task stop did not commit: {mutation:?}"));
            }
        }
        controller
            .install_surface(attachment.client.clone(), operation_id.clone())
            .map_err(|error| error.to_string())?;
        activation.disarm();
        guard.controller_installed();
        let outcome = drain_operation(
            &surface,
            &attachment.client,
            &operation_id,
            &mut subscription,
            &mut projection,
            controller,
            event_tx,
        )
        .map_err(|error| error.to_string())?;
        if !matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { ref status } if status == "backgrounded"
        ) {
            guard.terminal_observed();
        }
        let snapshot = read_snapshot(thread).map_err(|error| error.to_string())?;
        return Ok(SurfaceProjectionState::from_surface_snapshot(&snapshot));
    }
    if task.task_type != orca_runtime::surface::SurfaceTaskType::Workflow
        && task.workflow_run_id.is_none()
    {
        detach(&surface, &attachment.client);
        return Err(format!(
            "surface task '{task_id}' has no runtime-owned control"
        ));
    }
    let workflow = task
        .workflow_run_id
        .as_ref()
        .and_then(|run_id| {
            attachment
                .baseline
                .snapshot
                .workflows
                .iter()
                .find(|workflow| &workflow.workflow_run_id == run_id)
        })
        .cloned();
    let result = match workflow {
        Some(workflow) => attachment.client.workflow_control(
            SurfaceRequestId::new(),
            WorkflowControlAction::stop(&workflow),
        ),
        None => {
            detach(&surface, &attachment.client);
            return Err(format!(
                "surface task '{task_id}' has no runtime-owned workflow"
            ));
        }
    };
    detach(&surface, &attachment.client);
    let result = result.map_err(|error| format!("typed TUI task stop failed: {error:?}"))?;
    match result {
        MutationReply::Committed { .. } => {}
        MutationReply::Deferred { mutation, .. } => {
            return Err(format!(
                "typed TUI task stop deferred: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            ));
        }
        MutationReply::Uncommitted { mutation } => {
            return Err(format!("typed TUI task stop did not commit: {mutation:?}"));
        }
    }

    let snapshot = read_snapshot(thread).map_err(|error| error.to_string())?;
    Ok(SurfaceProjectionState::from_surface_snapshot(&snapshot))
}

pub(crate) fn foreground_task(
    thread: &RuntimeSurfaceThreadHandle,
    task_id: &str,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<SurfaceProjectionState, String> {
    let mut activation =
        SurfaceActivationGuard::begin(controller).map_err(|error| error.to_string())?;
    let surface = thread.surface();
    let deadline = Instant::now() + Duration::from_millis(500);
    let (attachment, task) = loop {
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::ManageTask,
                SurfaceCapability::ControlBoundOperation,
                SurfaceCapability::RespondGrantedInteraction,
            ]),
            interaction_capabilities: BTreeSet::from([
                SurfaceInteractionKind::ToolApproval,
                SurfaceInteractionKind::PermissionRequest,
                SurfaceInteractionKind::UserInput,
                SurfaceInteractionKind::McpElicitation,
            ]),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            AttachResult::Denied { reason } => {
                return Err(format!(
                    "typed TUI task-foreground attachment denied: {reason:?}"
                ));
            }
            AttachResult::Unavailable { reason } => {
                return Err(format!(
                    "typed TUI task-foreground attachment unavailable: {reason:?}"
                ));
            }
            AttachResult::ThreadClosed { .. } => {
                return Err("typed TUI task-foreground thread is closed".to_string());
            }
            AttachResult::CursorAttached { .. }
            | AttachResult::SnapshotRequired { .. }
            | AttachResult::InvalidCursor { .. } => {
                return Err(
                    "typed TUI task-foreground attachment returned an invalid fresh-attach result"
                        .to_string(),
                );
            }
        };
        if let Some(task) = attachment
            .baseline
            .snapshot
            .tasks
            .iter()
            .find(|task| task.task_id.as_str() == task_id)
            .cloned()
        {
            break (attachment, task);
        }
        detach(&surface, &attachment.client);
        if Instant::now() >= deadline {
            return Err(format!("surface task '{task_id}' not found"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let operation_id = task
        .parent_operation
        .clone()
        .ok_or_else(|| format!("surface task '{task_id}' has no owning operation"))?;
    controller.retire_surface_presentation(&operation_id);
    let mut guard = SurfaceRunGuard::new(&surface, attachment.client.clone(), controller);
    guard.bind_operation(operation_id.clone());
    guard.preserve_operation();
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| "typed TUI task-foreground subscription unavailable".to_string())?;
    let mut projection = TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
    projection.focus_operation(operation_id.clone());
    let delivery_watermark = controller.surface_delivery_watermark(&operation_id);
    let terminal_status = projection.terminal_status_for_operation(&operation_id);
    if terminal_status.is_some() && controller.surface_terminal_was_delivered(&operation_id) {
        return Err(format!(
            "surface task '{task_id}' terminal output was already delivered"
        ));
    }
    if terminal_status.is_none() && task.backgrounded {
        let result = attachment.client.task_control(
            SurfaceRequestId::new(),
            TaskControlAction::Foreground {
                fence: SurfaceTaskFence {
                    task_id: task.task_id,
                    task_revision: task.revision,
                    background_owner: task.background_fence,
                },
            },
        );
        let result =
            result.map_err(|error| format!("typed TUI task foreground failed: {error:?}"))?;
        match result {
            MutationReply::Committed { .. } => {}
            MutationReply::Deferred { mutation, .. } => {
                return Err(format!(
                    "typed TUI task foreground deferred: request={:?} commit={:?}",
                    mutation.request_id, mutation.commit_id
                ));
            }
            MutationReply::Uncommitted { mutation } => {
                return Err(format!(
                    "typed TUI task foreground did not commit: {mutation:?}"
                ));
            }
        }
    } else if terminal_status.is_none()
        && !task.backgrounded
        && !projection.operation_is_runtime_backgrounded(&operation_id)
        && attachment
            .baseline
            .snapshot
            .foreground_operation
            .as_ref()
            .is_none_or(|operation| operation.operation_id != operation_id)
    {
        return Err(format!(
            "surface task '{task_id}' is neither backgrounded nor attached to a runtime background owner"
        ));
    }
    let _ = event_tx.send(TuiEvent::BackgroundTaskOutputAttached {
        task_id: task_id.to_string(),
    });
    for event in projection
        .hydrate_after_delivery_watermark(&operation_id, &delivery_watermark)
        .map_err(|error| format!("typed TUI foreground hydration failed: {error:?}"))?
    {
        let _ = event_tx.send(event);
    }
    if let Some(status) = terminal_status {
        controller.remember_surface_delivery_watermark(
            operation_id.clone(),
            projection.delivery_watermark(&operation_id),
        );
        controller.remember_surface_terminal_delivery(operation_id);
        let _ = event_tx.send(TuiEvent::SessionCompleted {
            status: status.to_string(),
        });
        return Ok(SurfaceProjectionState::from_surface_snapshot(
            &attachment.baseline.snapshot,
        ));
    }
    controller
        .install_surface(attachment.client.clone(), operation_id.clone())
        .map_err(|error| error.to_string())?;
    activation.disarm();
    guard.controller_installed();
    let outcome = drain_operation(
        &surface,
        &attachment.client,
        &operation_id,
        &mut subscription,
        &mut projection,
        controller,
        event_tx,
    )
    .map_err(|error| error.to_string())?;
    if !matches!(
        outcome,
        TuiHostedOperationOutcome::Turn { ref status } if status == "backgrounded"
    ) {
        guard.terminal_observed();
    }
    let snapshot = read_snapshot(thread).map_err(|error| error.to_string())?;
    Ok(SurfaceProjectionState::from_surface_snapshot(&snapshot))
}

pub(crate) fn resolve_background_approval(
    thread: &RuntimeSurfaceThreadHandle,
    approval_id: &str,
    approved: bool,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(String, SurfaceProjectionState), String> {
    let surface = thread.surface();
    let deadline = Instant::now() + Duration::from_secs(5);
    let (attachment, interaction) = loop {
        let attachment = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::ManageTask,
                SurfaceCapability::ControlBoundOperation,
                SurfaceCapability::RespondGrantedInteraction,
            ]),
            interaction_capabilities: BTreeSet::from([
                SurfaceInteractionKind::ToolApproval,
                SurfaceInteractionKind::PermissionRequest,
                SurfaceInteractionKind::UserInput,
                SurfaceInteractionKind::McpElicitation,
                SurfaceInteractionKind::BackgroundApproval,
            ]),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            _ => return Err("background approval responder attachment unavailable".to_string()),
        };
        let interaction = attachment
            .baseline
            .snapshot
            .interactions
            .iter()
            .find(|interaction| {
                matches!(
                    &interaction.request,
                    orca_runtime::surface::SurfaceInteractionRequest::BackgroundApproval {
                        tool,
                        ..
                    } if tool.tool_call_id.as_str() == approval_id
                )
            })
            .cloned();
        if let Some(interaction) = interaction {
            break (attachment, interaction);
        }
        let last_observed = format!(
            "tasks={} interactions={} background={} history={}",
            attachment.baseline.snapshot.tasks.len(),
            attachment.baseline.snapshot.interactions.len(),
            attachment.baseline.snapshot.background_operations.len(),
            attachment.baseline.snapshot.operation_history.len(),
        );
        detach(&surface, &attachment.client);
        if Instant::now() >= deadline {
            return Err(format!(
                "pending background approval '{approval_id}' did not become durable; {}",
                last_observed
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| "background approval subscription unavailable".to_string())?;
    let task_fence = match &interaction.request {
        orca_runtime::surface::SurfaceInteractionRequest::BackgroundApproval { task, .. } => {
            task.clone()
        }
        _ => return Err("background approval request kind is invalid".to_string()),
    };
    let task_id = task_fence.task_id.as_str().to_string();
    let task = attachment
        .baseline
        .snapshot
        .tasks
        .iter()
        .find(|task| task.task_id == task_fence.task_id)
        .cloned()
        .ok_or_else(|| "background approval task disappeared".to_string())?;
    let operation_id = task
        .parent_operation
        .clone()
        .ok_or_else(|| "background approval task has no owning operation".to_string())?;
    let mut foreground_run = if approved {
        let mut activation =
            SurfaceActivationGuard::begin(controller).map_err(|error| error.to_string())?;
        controller.retire_surface_presentation(&operation_id);
        let mut guard = SurfaceRunGuard::new(&surface, attachment.client.clone(), controller);
        guard.bind_operation(operation_id.clone());
        guard.preserve_operation();
        if task.backgrounded {
            match attachment.client.task_control(
                SurfaceRequestId::new(),
                TaskControlAction::Foreground {
                    fence: SurfaceTaskFence {
                        task_id: task.task_id.clone(),
                        task_revision: task.revision,
                        background_owner: task.background_fence.clone(),
                    },
                },
            ) {
                Ok(MutationReply::Committed { .. }) => {}
                Ok(MutationReply::Deferred { mutation, .. }) => {
                    return Err(format!(
                        "background approval task foreground deferred: request={:?} commit={:?}",
                        mutation.request_id, mutation.commit_id
                    ));
                }
                Ok(MutationReply::Uncommitted { mutation }) => {
                    return Err(format!(
                        "background approval task foreground did not commit: {mutation:?}"
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "background approval task foreground failed: {error:?}"
                    ));
                }
            }
        }
        let mut projection =
            TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
        projection.focus_operation(operation_id.clone());
        let delivery_watermark = controller.surface_delivery_watermark(&operation_id);
        let _ = event_tx.send(TuiEvent::BackgroundTaskOutputAttached {
            task_id: task_id.clone(),
        });
        for event in projection
            .hydrate_after_delivery_watermark(&operation_id, &delivery_watermark)
            .map_err(|error| format!("typed TUI approval hydration failed: {error:?}"))?
        {
            let _ = event_tx.send(event);
        }
        controller
            .install_surface(attachment.client.clone(), operation_id.clone())
            .map_err(|error| error.to_string())?;
        activation.disarm();
        guard.controller_installed();
        Some((guard, projection))
    } else {
        None
    };
    let decision = if approved {
        SurfaceAllowDeny::Allow
    } else {
        SurfaceAllowDeny::Deny
    };
    let result = loop {
        match attachment.client.respond_interaction_by_id(
            SurfaceRequestId::new(),
            interaction.interaction_id.clone(),
            SurfaceClientInteractionAnswer::BackgroundApproval { decision },
        ) {
            Err(orca_runtime::surface::SurfaceClientCommandError::RuntimeUnavailable)
                if Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => break result,
        }
    }
    .map_err(|error| format!("background approval response failed: {error:?}"));
    match result? {
        MutationReply::Committed { .. } => {}
        MutationReply::Deferred { .. } => {
            return Err("background approval response was deferred".to_string());
        }
        MutationReply::Uncommitted { mutation } => {
            return Err(format!(
                "background approval response was not committed: {mutation:?}"
            ));
        }
    }
    if let Some((mut guard, mut projection)) = foreground_run.take() {
        projection.focus_operation(operation_id.clone());
        let outcome = drain_operation(
            &surface,
            &attachment.client,
            &operation_id,
            &mut subscription,
            &mut projection,
            controller,
            event_tx,
        )
        .map_err(|error| error.to_string())?;
        if !matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { ref status } if status == "backgrounded"
        ) {
            guard.terminal_observed();
        }
    } else {
        detach(&surface, &attachment.client);
    }
    let snapshot = read_snapshot(thread).map_err(|error| error.to_string())?;
    Ok((
        task_id,
        SurfaceProjectionState::from_surface_snapshot(&snapshot),
    ))
}

pub(crate) fn read_goal(
    thread: &RuntimeSurfaceThreadHandle,
) -> io::Result<Option<orca_core::goal_types::ThreadGoal>> {
    let snapshot = read_snapshot(thread)?;
    Ok(snapshot.goal.as_ref().map(|goal| {
        crate::surface_projection::thread_goal_from_surface(
            goal,
            snapshot.thread.created_at,
            snapshot.thread.updated_at,
        )
    }))
}

pub(crate) fn edit_goal(
    thread: &RuntimeSurfaceThreadHandle,
    objective: String,
) -> io::Result<SurfaceProjectionState> {
    mutate_idle_goal(thread, |goal| {
        Ok(GoalMutationAction::Edit {
            fence: goal_fence(goal),
            objective: NonEmptyText::try_new(objective)
                .map_err(|error| io::Error::other(error.to_string()))?,
            token_budget: GoalTokenBudgetUpdate::Keep,
        })
    })
}

pub(crate) fn clear_goal(
    thread: &RuntimeSurfaceThreadHandle,
) -> io::Result<SurfaceProjectionState> {
    mutate_idle_goal(thread, |goal| {
        Ok(GoalMutationAction::Clear {
            fence: goal_fence(goal),
        })
    })
}

pub(crate) fn pause_goal(
    thread: &RuntimeSurfaceThreadHandle,
) -> io::Result<SurfaceProjectionState> {
    let surface = thread.surface();
    let attachment = attach_goal(&surface, false)?;
    let snapshot = &attachment.baseline.snapshot;
    let current = snapshot
        .goal
        .as_ref()
        .ok_or_else(|| io::Error::other("no goal is currently set"))?;
    let result = attachment
        .client
        .pause_goal_operation(SurfaceRequestId::new(), goal_fence(current));
    detach(&surface, &attachment.client);
    let output = match result
        .map_err(|error| io::Error::other(format!("typed TUI Goal pause failed: {error:?}")))?
    {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred { mutation, .. } => {
            return Err(io::Error::other(format!(
                "typed TUI Goal pause deferred: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )));
        }
        MutationReply::Uncommitted { mutation } => {
            return Err(io::Error::other(format!(
                "typed TUI Goal pause did not commit: {mutation:?}"
            )));
        }
    };
    committed_goal_projection(thread, &output.goal_cursor)
}

fn mutate_idle_goal(
    thread: &RuntimeSurfaceThreadHandle,
    action: impl FnOnce(&SurfaceGoal) -> io::Result<GoalMutationAction>,
) -> io::Result<SurfaceProjectionState> {
    let surface = thread.surface();
    let attachment = attach_goal(&surface, false)?;
    let snapshot = &attachment.baseline.snapshot;
    let current = snapshot
        .goal
        .as_ref()
        .ok_or_else(|| io::Error::other("no goal is currently set"))?;
    let result = attachment
        .client
        .goal_mutation(SurfaceRequestId::new(), action(current)?);
    detach(&surface, &attachment.client);
    let output = committed_goal_output(
        result.map_err(|error| io::Error::other(format!("typed TUI Goal failed: {error:?}")))?,
    )?;
    committed_goal_projection(thread, &output.change_cursor)
}

fn committed_goal_projection(
    thread: &RuntimeSurfaceThreadHandle,
    change_cursor: &orca_runtime::surface::SurfaceCursor,
) -> io::Result<SurfaceProjectionState> {
    let snapshot = read_snapshot(thread).map_err(|error| {
        io::Error::other(format!(
            "Goal mutation committed but TUI projection failed: {error}"
        ))
    })?;
    if !goal_projection_cursor_covers_commit(&snapshot.cursor, change_cursor) {
        return Err(io::Error::other(
            "Goal mutation committed but TUI projection failed: fresh snapshot did not include the committed cursor",
        ));
    }
    let projection = SurfaceProjectionState::from_surface_snapshot(&snapshot);
    let presentation = if projection.current_goal.is_some() {
        GoalProjectionPresentation::Updated
    } else {
        GoalProjectionPresentation::Cleared
    };
    Ok(projection.with_goal_presentation(presentation))
}

fn goal_projection_cursor_covers_commit(
    snapshot_cursor: &orca_runtime::surface::SurfaceCursor,
    committed_cursor: &orca_runtime::surface::SurfaceCursor,
) -> bool {
    snapshot_cursor.thread_id == committed_cursor.thread_id
        && snapshot_cursor.incarnation == committed_cursor.incarnation
        && snapshot_cursor.next_seq >= committed_cursor.next_seq
}

pub(crate) fn set_goal_and_run(
    thread: &RuntimeSurfaceThreadHandle,
    objective: String,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    run_goal_mutation(
        thread,
        controller,
        event_tx,
        || {},
        move |snapshot| {
            let objective = NonEmptyText::try_new(objective)
                .map_err(|error| io::Error::other(error.to_string()))?;
            Ok(GoalMutationAction::SetAndRun {
                expected_goal: snapshot
                    .goal
                    .as_ref()
                    .map(|goal| ExpectedGoal::Exact(goal_fence(goal)))
                    .unwrap_or(ExpectedGoal::None),
                token_budget: snapshot.goal.as_ref().and_then(|goal| goal.token_budget),
                input: supplied_goal_input(objective.as_str())?,
                objective,
            })
        },
    )
}

pub(crate) fn resume_goal_and_run(
    thread: &RuntimeSurfaceThreadHandle,
    prompt: String,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    run_goal_mutation(
        thread,
        controller,
        event_tx,
        || {},
        move |snapshot| {
            let goal = snapshot
                .goal
                .as_ref()
                .ok_or_else(|| io::Error::other("no goal is currently set"))?;
            Ok(GoalMutationAction::ResumeAndRun {
                fence: goal_fence(goal),
                input: supplied_goal_input(&prompt)?,
            })
        },
    )
}

pub(crate) fn resume_goal_and_run_with_started(
    thread: &RuntimeSurfaceThreadHandle,
    prompt: String,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
    started: impl FnOnce(),
) -> io::Result<TuiHostedOperationOutcome> {
    run_goal_mutation(thread, controller, event_tx, started, move |snapshot| {
        let goal = snapshot
            .goal
            .as_ref()
            .ok_or_else(|| io::Error::other("no goal is currently set"))?;
        Ok(GoalMutationAction::ResumeAndRun {
            fence: goal_fence(goal),
            input: supplied_goal_input(&prompt)?,
        })
    })
}

fn run_goal_mutation(
    thread: &RuntimeSurfaceThreadHandle,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
    started: impl FnOnce(),
    action: impl FnOnce(&SurfaceSnapshot) -> io::Result<GoalMutationAction>,
) -> io::Result<TuiHostedOperationOutcome> {
    let mut activation = SurfaceActivationGuard::begin(controller)?;
    let surface = thread.surface();
    let attachment = attach_goal(&surface, true)?;
    let mut guard = SurfaceRunGuard::new(&surface, attachment.client.clone(), controller);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| io::Error::other("typed TUI Goal subscription unavailable"))?;
    let mut projection = TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
    let output = committed_goal_output(
        attachment
            .client
            .goal_mutation(
                SurfaceRequestId::new(),
                action(&attachment.baseline.snapshot)?,
            )
            .map_err(|error| io::Error::other(format!("typed TUI Goal failed: {error:?}")))?,
    )?;
    let goal = output
        .goal
        .as_ref()
        .ok_or_else(|| io::Error::other("typed TUI Goal mutation removed the goal"))?;
    let operation_id = output
        .operation_id
        .ok_or_else(|| io::Error::other("typed TUI Goal mutation did not start an operation"))?;
    guard.bind_operation(operation_id.clone());
    projection.focus_operation(operation_id.clone());
    controller.install_surface_goal(
        attachment.client.clone(),
        operation_id.clone(),
        goal_fence(goal),
    )?;
    started();
    activation.disarm();
    guard.controller_installed();
    let result = drain_operation(
        &surface,
        &attachment.client,
        &operation_id,
        &mut subscription,
        &mut projection,
        controller,
        event_tx,
    );
    if result.is_ok() {
        guard.terminal_observed();
    }
    result
}

fn attach_goal(
    surface: &RuntimeSurfaceHandle,
    running: bool,
) -> io::Result<orca_runtime::surface::FreshSurfaceAttachment> {
    let mut capabilities = BTreeSet::from([
        SurfaceCapability::ReadSnapshot,
        SurfaceCapability::ManageGoal,
    ]);
    let mut interaction_capabilities = BTreeSet::new();
    if running {
        capabilities.extend([
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::RespondGrantedInteraction,
        ]);
        interaction_capabilities.extend([
            SurfaceInteractionKind::ToolApproval,
            SurfaceInteractionKind::PermissionRequest,
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionKind::McpElicitation,
        ]);
    }
    match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: capabilities,
        interaction_capabilities,
    }) {
        AttachResult::FreshAttached { attachment } => Ok(attachment),
        _ => Err(io::Error::other("typed TUI Goal attachment unavailable")),
    }
}

fn committed_goal_output(
    reply: MutationReply<orca_runtime::surface::GoalMutationOutput>,
) -> io::Result<orca_runtime::surface::GoalMutationOutput> {
    match reply {
        MutationReply::Committed { value, .. } => Ok(value),
        MutationReply::Deferred { mutation, .. } => Err(io::Error::other(format!(
            "typed TUI Goal mutation deferred: request={:?} commit={:?}",
            mutation.request_id, mutation.commit_id
        ))),
        MutationReply::Uncommitted { mutation } => Err(io::Error::other(format!(
            "typed TUI Goal mutation did not commit: {mutation:?}"
        ))),
    }
}

fn supplied_goal_input(prompt: &str) -> io::Result<GoalRunInput> {
    Ok(GoalRunInput::Supplied {
        request: SurfaceInputRequest {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                text: DisplayText::new(prompt),
            }])
            .map_err(|error| io::Error::other(error.to_string()))?,
        },
    })
}

fn goal_fence(goal: &SurfaceGoal) -> SurfaceGoalFence {
    SurfaceGoalFence {
        goal_id: goal.goal_id.clone(),
        goal_revision: goal.goal_revision,
        goal_owner_epoch: goal.goal_owner_epoch,
    }
}

pub(crate) fn launch_workflow(
    thread: &RuntimeSurfaceThreadHandle,
    name: &str,
    raw_args: Option<&str>,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<()> {
    if thread.session_id().is_none() {
        return Err(io::Error::other(
            "typed workflow launch requires recorded conversation history",
        ));
    }
    let surface = thread.surface();
    let attach_deadline = Instant::now() + Duration::from_secs(5);
    let attachment = loop {
        match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::ControlBoundOperation,
                SurfaceCapability::ManageWorkflow,
            ]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => break attachment,
            AttachResult::Unavailable {
                reason: SurfaceUnavailableReason::RuntimeUnavailable,
            } if Instant::now() < attach_deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            AttachResult::Denied { reason } => {
                return Err(io::Error::other(format!(
                    "typed TUI workflow attachment denied: {reason:?}"
                )));
            }
            AttachResult::Unavailable { reason } => {
                return Err(io::Error::other(format!(
                    "typed TUI workflow attachment unavailable: {reason:?}"
                )));
            }
            AttachResult::ThreadClosed { .. } => {
                return Err(io::Error::other(
                    "typed TUI workflow attachment found a closed thread",
                ));
            }
            AttachResult::CursorAttached { .. }
            | AttachResult::SnapshotRequired { .. }
            | AttachResult::InvalidCursor { .. } => {
                return Err(io::Error::other(
                    "typed TUI workflow fresh attachment returned an invalid result",
                ));
            }
        }
    };
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| io::Error::other("typed TUI workflow subscription unavailable"))?;
    let mut projection = TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
    let result = attachment.client.workflow_control(
        SurfaceRequestId::new(),
        WorkflowControlAction::Launch {
            catalog_entry_id: SurfaceCatalogEntryId::try_new(name)
                .map_err(|error| io::Error::other(error.to_string()))?,
            observed_catalog_revision: WorkflowCatalogRevision::try_new(1)
                .expect("initial workflow catalog revision is positive"),
            args: parse_workflow_args(raw_args)?,
            parent: None,
        },
    );
    let output = match result
        .map_err(|error| io::Error::other(format!("typed TUI workflow launch failed: {error:?}")))?
    {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred { mutation, .. } => {
            detach(&surface, &attachment.client);
            return Err(io::Error::other(format!(
                "typed TUI workflow launch deferred: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )));
        }
        MutationReply::Uncommitted { mutation } => {
            detach(&surface, &attachment.client);
            return Err(io::Error::other(format!(
                "typed TUI workflow launch did not commit: {mutation:?}"
            )));
        }
    };
    let operation_id = output
        .operation_id
        .ok_or_else(|| io::Error::other("typed TUI workflow launch has no operation"))?;
    let monitor_client = attachment.client.clone();
    let monitor_surface = surface.clone();
    let monitor_events = event_tx.clone();
    let monitor_operation_id = operation_id.clone();
    let monitor_workflow_run_id = output.workflow.workflow_run_id.clone();
    std::thread::Builder::new()
        .name(format!("tui-workflow-{}", output.workflow.task_id.as_str()))
        .spawn(move || {
            let mut notification_sent = false;
            loop {
                let Some(item) = subscription.recv_timeout(Duration::from_millis(100)) else {
                    continue;
                };
                match item {
                    SurfaceSubscriptionItem::Batch { batch } => {
                        let terminal = batch.events.as_slice().iter().any(|envelope| {
                            matches!(
                                &envelope.event,
                                SurfaceEvent::Operation(OperationPatch::Terminal { record })
                                    if record.operation_id == monitor_operation_id
                            )
                        });
                        let workflow_terminal =
                            batch_contains_workflow_terminal(&batch, &monitor_workflow_run_id);
                        match projection.project_typed_batch(&batch) {
                            Ok(events) => {
                                for event in events {
                                    let _ = monitor_events.send(event);
                                }
                                if workflow_terminal && !notification_sent {
                                    if let Some(event) = projection
                                        .terminal_workflow_notification(&monitor_workflow_run_id)
                                    {
                                        let _ = monitor_events.send(event);
                                        notification_sent = true;
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = monitor_events.send(TuiEvent::Error(format!(
                                    "typed TUI workflow projection failed: {error:?}"
                                )));
                                let _ = monitor_client.cancel_operation(
                                    SurfaceRequestId::new(),
                                    monitor_operation_id.clone(),
                                );
                                break;
                            }
                        }
                        if terminal {
                            break;
                        }
                    }
                    SurfaceSubscriptionItem::Gap { required } => {
                        let _ = monitor_events.send(TuiEvent::Error(format!(
                            "typed TUI workflow subscription gap requires {:?}",
                            required.reason
                        )));
                        let _ = monitor_client.cancel_operation(
                            SurfaceRequestId::new(),
                            monitor_operation_id.clone(),
                        );
                        break;
                    }
                    SurfaceSubscriptionItem::Sealed { .. } => break,
                }
            }
            detach(&monitor_surface, &monitor_client);
        })
        .map_err(|error| {
            let _ = attachment
                .client
                .cancel_operation(SurfaceRequestId::new(), operation_id);
            detach(&surface, &attachment.client);
            io::Error::other(format!(
                "failed to start typed TUI workflow monitor: {error}"
            ))
        })?;
    Ok(())
}

fn batch_contains_workflow_terminal(
    batch: &orca_runtime::surface::SurfaceCommitBatch,
    workflow_run_id: &SurfaceWorkflowRunId,
) -> bool {
    batch.events.as_slice().iter().any(|event| {
        matches!(
            &event.event,
            SurfaceEvent::Workflow(
                WorkflowPatch::Completed { fence, .. }
                    | WorkflowPatch::Failed { fence, .. }
                    | WorkflowPatch::Stopped { fence, .. }
                    | WorkflowPatch::Cancelled { fence, .. }
            ) if &fence.workflow_run_id == workflow_run_id
        )
    })
}

fn parse_workflow_args(raw_args: Option<&str>) -> io::Result<Vec<(NonEmptyText, DisplayText)>> {
    let Some(raw) = raw_args.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut args = std::collections::BTreeMap::<String, serde_json::Value>::new();
    if raw.starts_with('{') {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|error| io::Error::other(error.to_string()))?;
        let object = value
            .as_object()
            .ok_or_else(|| io::Error::other("workflow args JSON must be an object"))?;
        for (name, value) in object {
            args.insert(name.clone(), value.clone());
        }
    } else {
        for part in raw.split_whitespace() {
            let (name, value) = part.split_once('=').ok_or_else(|| {
                io::Error::other(format!("workflow arg `{part}` must use key=value"))
            })?;
            if name.trim().is_empty() {
                return Err(io::Error::other("workflow arg key cannot be empty"));
            }
            let value = serde_json::from_str(value)
                .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
            if args.insert(name.to_string(), value).is_some() {
                return Err(io::Error::other(format!(
                    "workflow arg `{name}` was provided more than once"
                )));
            }
        }
    }
    args.into_iter()
        .map(|(name, value)| {
            Ok((
                NonEmptyText::try_new(name).map_err(|error| io::Error::other(error.to_string()))?,
                DisplayText::new(value.to_string()),
            ))
        })
        .collect()
}

pub(crate) fn add_pinned_context(
    thread: &RuntimeSurfaceThreadHandle,
    note: &str,
) -> io::Result<()> {
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManagePinnedContext,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => {
            return Err(io::Error::other(
                "typed TUI pinned context attachment unavailable",
            ));
        }
    };
    let revision = attachment.baseline.snapshot.pinned_context.revision;
    let entry = SurfacePinnedContextEntry {
        id: SurfaceCatalogEntryId::try_new(format!(
            "tui-note-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
        .map_err(|error| io::Error::other(error.to_string()))?,
        kind: SurfacePinnedContextKind::User,
        label: orca_runtime::surface::NonEmptyText::try_new("remembered note")
            .map_err(|error| io::Error::other(error.to_string()))?,
        content: DisplayText::new(note),
        content_digest: Sha256Digest::digest(note.as_bytes()),
        source_revision: PinnedContextSourceRevision::User(
            PinnedUserRevision::try_new(1).expect("user pinned revision is positive"),
        ),
    };
    let result = attachment.client.pinned_context_mutation(
        SurfaceRequestId::new(),
        PinnedContextAction::Add {
            expected_revision: revision,
            entry,
            memory_receipt: None,
        },
    );
    detach(&surface, &attachment.client);
    match result
        .map_err(|error| io::Error::other(format!("typed TUI pinned context failed: {error:?}")))?
    {
        MutationReply::Committed { .. } => Ok(()),
        MutationReply::Uncommitted { mutation } => Err(io::Error::other(format!(
            "typed TUI pinned context did not commit: {mutation:?}"
        ))),
        MutationReply::Deferred { .. } => {
            Err(io::Error::other("typed TUI pinned context deferred"))
        }
    }
}

fn run_typed_thread(
    thread: &RuntimeSurfaceThreadHandle,
    request: HostedTurnRequest,
    config: RunConfig,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    let surface = thread.surface();
    run_typed_surface(&surface, request, config, controller, event_tx)
}

fn run_typed_surface(
    surface: &RuntimeSurfaceHandle,
    request: HostedTurnRequest,
    config: RunConfig,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    let mut activation = SurfaceActivationGuard::begin(controller)?;
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::RespondGrantedInteraction,
        ]),
        interaction_capabilities: BTreeSet::from([
            SurfaceInteractionKind::ToolApproval,
            SurfaceInteractionKind::PermissionRequest,
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionKind::McpElicitation,
        ]),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. } => {
            return Err(io::Error::other("typed TUI surface attachment denied"));
        }
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            return Err(io::Error::other("typed TUI surface unavailable"));
        }
    };
    let mut guard = SurfaceRunGuard::new(&surface, attachment.client.clone(), controller);
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| io::Error::other("typed TUI surface subscription unavailable"))?;
    let mut projection = TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
    if !typed_config_matches_surface(&config, &attachment.baseline.snapshot) {
        return Err(io::Error::other(
            "typed TUI config differs from runtime surface settings; update settings before submitting",
        ));
    }
    for event in projection.hydrate_open_streams() {
        let _ = event_tx.send(event);
    }
    let intent = OperationRequestIntent {
        correlation: OperationIngressCorrelation::TuiUser,
        kind: OperationKind::UserTurn,
        input: Some(SurfaceInputRequest {
            blocks: orca_runtime::surface::NonEmptyVec::try_new(vec![
                SurfaceInputRequestBlock::Text {
                    text: orca_runtime::surface::DisplayText::new(request.prompt()),
                },
            ])
            .map_err(|error| io::Error::other(error.to_string()))?,
        }),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: attachment.baseline.snapshot.settings.thread_revision,
            expected_policy_epoch: attachment.baseline.snapshot.settings.effective.policy_epoch,
        },
    };
    let reserved = match attachment
        .client
        .reserve_operation(SurfaceRequestId::new(), intent)
        .map_err(|error| io::Error::other(format!("typed TUI reserve failed: {error:?}")))?
    {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred {
            mutation,
            partial: orca_runtime::surface::DeferredCommandValue::Provisional { value },
        } => {
            guard.bind_operation(value.operation_id.clone());
            return Err(io::Error::other(format!(
                "typed TUI reserve deferred and requires runtime reconciliation: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )));
        }
        MutationReply::Deferred {
            mutation,
            partial: orca_runtime::surface::DeferredCommandValue::NoValue,
        } => {
            return Err(io::Error::other(format!(
                "typed TUI reserve deferred without provisional operation: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )));
        }
        MutationReply::Uncommitted { mutation } => {
            return Err(io::Error::other(format!(
                "typed TUI reserve did not commit: {mutation:?}"
            )));
        }
    };
    let operation_id = reserved.operation_id.clone();
    guard.bind_operation(operation_id.clone());
    projection.focus_operation(operation_id.clone());
    match attachment
        .client
        .admit_reserved(
            SurfaceRequestId::new(),
            operation_id.clone(),
            reserved.lease.lease_id,
        )
        .map_err(|error| io::Error::other(format!("typed TUI admission failed: {error:?}")))?
    {
        MutationReply::Committed { .. } => {}
        MutationReply::Deferred { mutation, .. } => {
            return Err(io::Error::other(format!(
                "typed TUI admission deferred and requires runtime reconciliation: request={:?} commit={:?}",
                mutation.request_id, mutation.commit_id
            )));
        }
        MutationReply::Uncommitted { mutation } => {
            return Err(io::Error::other(format!(
                "typed TUI admission did not commit: {mutation:?}"
            )));
        }
    }
    controller.install_surface(attachment.client.clone(), operation_id.clone())?;
    activation.disarm();
    guard.controller_installed();

    let result = drain_operation(
        &surface,
        &attachment.client,
        &operation_id,
        &mut subscription,
        &mut projection,
        controller,
        event_tx,
    );
    if result.is_ok() {
        guard.terminal_observed();
    }
    result
}

#[derive(Clone, Copy)]
enum RecoveryControl {
    Resume,
    Cancel,
}

fn control_recovered_operation(
    thread: &RuntimeSurfaceThreadHandle,
    expected_operation_id: &SurfaceOperationId,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
    control: RecoveryControl,
) -> io::Result<TuiHostedOperationOutcome> {
    let mut activation = SurfaceActivationGuard::begin(controller)?;
    let surface = thread.surface();
    let attachment = match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::RespondGrantedInteraction,
        ]),
        interaction_capabilities: BTreeSet::from([
            SurfaceInteractionKind::ToolApproval,
            SurfaceInteractionKind::PermissionRequest,
            SurfaceInteractionKind::UserInput,
            SurfaceInteractionKind::McpElicitation,
        ]),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        AttachResult::Denied { .. } => {
            return Err(io::Error::other("typed TUI recovery attachment denied"));
        }
        AttachResult::CursorAttached { .. }
        | AttachResult::SnapshotRequired { .. }
        | AttachResult::InvalidCursor { .. }
        | AttachResult::ThreadClosed { .. }
        | AttachResult::Unavailable { .. } => {
            return Err(io::Error::other(
                "typed TUI recovery attachment unavailable",
            ));
        }
    };
    let mut guard = SurfaceRunGuard::new(&surface, attachment.client.clone(), controller);
    guard.preserve_operation();
    let recovery = attachment
        .baseline
        .snapshot
        .recoverable_user_operation()
        .ok_or_else(|| io::Error::other("no recoverable operation is available"))?;
    let operation_id = recovery.operation_id().clone();
    if &operation_id != expected_operation_id {
        return Err(io::Error::other(
            "recoverable operation changed before the command was admitted",
        ));
    }
    guard.bind_operation(operation_id.clone());
    let mut subscription = surface
        .claim_subscription(&attachment.subscription)
        .ok_or_else(|| io::Error::other("typed TUI recovery subscription unavailable"))?;
    let mut projection = TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
    projection.focus_operation(operation_id.clone());
    for event in projection.hydrate_open_streams() {
        let _ = event_tx.send(event);
    }

    match control {
        RecoveryControl::Resume => {
            match attachment
                .client
                .resume_recoverable(SurfaceRequestId::new(), recovery)
                .map_err(|error| io::Error::other(format!("typed TUI resume failed: {error:?}")))?
            {
                MutationReply::Committed { .. } => guard.operation_started(),
                MutationReply::Deferred { mutation, .. } => {
                    return Err(io::Error::other(format!(
                        "typed TUI resume deferred and requires runtime reconciliation: request={:?} commit={:?}",
                        mutation.request_id, mutation.commit_id
                    )));
                }
                MutationReply::Uncommitted { mutation } => {
                    return Err(io::Error::other(format!(
                        "typed TUI resume did not commit: {mutation:?}"
                    )));
                }
            }
        }
        RecoveryControl::Cancel => {
            match attachment
                .client
                .cancel_operation(SurfaceRequestId::new(), operation_id.clone())
                .map_err(|error| {
                    io::Error::other(format!("typed TUI recovery cancel failed: {error:?}"))
                })? {
                MutationReply::Committed { .. } => {}
                MutationReply::Deferred { mutation, .. } => {
                    return Err(io::Error::other(format!(
                        "typed TUI recovery cancel deferred and requires runtime reconciliation: request={:?} commit={:?}",
                        mutation.request_id, mutation.commit_id
                    )));
                }
                MutationReply::Uncommitted { mutation } => {
                    return Err(io::Error::other(format!(
                        "typed TUI recovery cancel did not commit: {mutation:?}"
                    )));
                }
            }
        }
    }

    controller.install_surface(attachment.client.clone(), operation_id.clone())?;
    activation.disarm();
    guard.controller_installed();
    let result = drain_operation(
        &surface,
        &attachment.client,
        &operation_id,
        &mut subscription,
        &mut projection,
        controller,
        event_tx,
    );
    if result.is_ok() {
        guard.terminal_observed();
    }
    result
}

fn typed_config_matches_surface(
    config: &RunConfig,
    snapshot: &orca_runtime::surface::SurfaceSnapshot,
) -> bool {
    let expected_cwd = config.cwd.clone().unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
    });
    let Ok(expected_cwd) = orca_runtime::surface::CanonicalPath::try_new(expected_cwd) else {
        return false;
    };
    let expected_roots = config
        .runtime_workspace_roots
        .clone()
        .unwrap_or_else(|| vec![expected_cwd.as_path().to_path_buf()]);
    let expected_roots = expected_roots
        .into_iter()
        .map(orca_runtime::surface::CanonicalPath::try_new)
        .collect::<Result<Vec<_>, _>>();
    let Ok(expected_roots) = expected_roots else {
        return false;
    };
    let expected_approval_mode = match config.approval_mode {
        orca_core::approval_types::ApprovalMode::Suggest => {
            orca_runtime::surface::SurfaceApprovalMode::Suggest
        }
        orca_core::approval_types::ApprovalMode::AutoEdit => {
            orca_runtime::surface::SurfaceApprovalMode::AutoEdit
        }
        orca_core::approval_types::ApprovalMode::FullAuto => {
            orca_runtime::surface::SurfaceApprovalMode::FullAuto
        }
        orca_core::approval_types::ApprovalMode::Plan => {
            orca_runtime::surface::SurfaceApprovalMode::Plan
        }
    };
    let expected_reasoning = match config.reasoning_effort {
        orca_core::config::ReasoningEffort::Low => {
            orca_runtime::surface::SurfaceReasoningEffort::Low
        }
        orca_core::config::ReasoningEffort::High => {
            orca_runtime::surface::SurfaceReasoningEffort::High
        }
        orca_core::config::ReasoningEffort::Max => {
            orca_runtime::surface::SurfaceReasoningEffort::Max
        }
    };
    snapshot.settings.effective.model.as_str() == config.model.display_name()
        && snapshot.settings.effective.cwd == expected_cwd
        && snapshot.settings.effective.workspace_roots == expected_roots
        && snapshot.settings.effective.approval_mode == expected_approval_mode
        && snapshot.settings.effective.reasoning_effort == expected_reasoning
}

fn drain_operation(
    surface: &RuntimeSurfaceHandle,
    client: &RuntimeSurfaceClientHandle,
    operation_id: &SurfaceOperationId,
    subscription: &mut orca_runtime::surface::SurfaceSubscriptionReceiver,
    projection: &mut TuiSurfaceProjection,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> io::Result<TuiHostedOperationOutcome> {
    drain_operation_with_boundary(
        surface,
        client,
        operation_id,
        subscription,
        projection,
        controller,
        event_tx,
        false,
    )
}

struct TerminalWaiterGuard {
    cancellation: OptionalProcessLocalCancel,
    waiter: Option<std::thread::JoinHandle<()>>,
}

impl TerminalWaiterGuard {
    fn cancel_and_join(&mut self) {
        self.cancellation.cancel();
        self.join();
    }

    fn join(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            let _ = waiter.join();
        }
    }
}

impl Drop for TerminalWaiterGuard {
    fn drop(&mut self) {
        self.cancel_and_join();
    }
}

fn drain_operation_with_boundary(
    surface: &RuntimeSurfaceHandle,
    client: &RuntimeSurfaceClientHandle,
    operation_id: &SurfaceOperationId,
    subscription: &mut orca_runtime::surface::SurfaceSubscriptionReceiver,
    projection: &mut TuiSurfaceProjection,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
    defer_compacted_until_terminal: bool,
) -> io::Result<TuiHostedOperationOutcome> {
    let (wait_tx, wait_rx) = mpsc::bounded(1);
    let waiter_cancellation = OptionalProcessLocalCancel::new();
    let waiter_cancellation_for_thread = waiter_cancellation.clone();
    let waiter_client = client.clone();
    let waiter_operation_id = operation_id.clone();
    let waiter = std::thread::spawn(move || {
        let result = waiter_client.wait_operation_terminal_with_cancel(
            SurfaceRequestId::new(),
            waiter_operation_id,
            waiter_cancellation_for_thread,
        );
        let _ = wait_tx.send(result);
    });
    let mut waiter_guard = TerminalWaiterGuard {
        cancellation: waiter_cancellation,
        waiter: Some(waiter),
    };
    let mut terminal_seen = false;
    let mut sealed = false;
    let mut terminal_receipt = None;
    let mut failure: Option<io::Error> = None;
    let mut projected_terminal_event = None;
    let mut projected_compacted_event = None;
    while (!terminal_seen || terminal_receipt.is_none()) && !sealed {
        if projection.active_generation_fence(operation_id).is_some()
            && controller.begin_surface_background_handoff(operation_id)
        {
            let fence = projection
                .active_generation_fence(operation_id)
                .expect("background request checked an active generation");
            let transfer = client.transfer_background(
                SurfaceRequestId::new(),
                BackgroundTarget::ActiveGeneration { fence },
            );
            let transfer = match transfer {
                Ok(transfer) => transfer,
                Err(error) => {
                    controller.rollback_surface_background_handoff(operation_id);
                    return Err(io::Error::other(format!(
                        "typed TUI background transfer failed: {error:?}"
                    )));
                }
            };
            match transfer {
                MutationReply::Committed {
                    value: TransferBackgroundOutput::HandedOff { .. },
                    ..
                } => {
                    if !controller.commit_surface_background_handoff(operation_id) {
                        let _ = event_tx.send(TuiEvent::Error(
                            "typed TUI lost its pending background handoff registration after the runtime committed ownership"
                                .to_string(),
                        ));
                    }
                    controller.remember_surface_delivery_watermark(
                        operation_id.clone(),
                        projection.delivery_watermark(operation_id),
                    );
                    if let Err(error) = spawn_background_presentation(
                        surface,
                        operation_id.clone(),
                        controller,
                        event_tx.clone(),
                    ) {
                        let _ = event_tx.send(TuiEvent::Error(format!(
                            "typed TUI could not supervise background presentation: {error}"
                        )));
                    }
                    return Ok(TuiHostedOperationOutcome::Turn {
                        status: "backgrounded".to_string(),
                    });
                }
                MutationReply::Committed {
                    value: TransferBackgroundOutput::QueuedOnStart { .. },
                    ..
                } => {
                    controller.rollback_surface_background_handoff(operation_id);
                    failure = Some(io::Error::other(
                        "typed TUI active turn returned a queued background intent",
                    ));
                    sealed = true;
                }
                MutationReply::Deferred { mutation, .. } => {
                    controller.rollback_surface_background_handoff(operation_id);
                    failure = Some(io::Error::other(format!(
                        "typed TUI background transfer deferred: request={:?} commit={:?}",
                        mutation.request_id, mutation.commit_id
                    )));
                    sealed = true;
                }
                MutationReply::Uncommitted { mutation } => {
                    controller.rollback_surface_background_handoff(operation_id);
                    failure = Some(io::Error::other(format!(
                        "typed TUI background transfer did not commit: {mutation:?}"
                    )));
                    sealed = true;
                }
            }
        }
        let mut next_item = subscription.try_recv();
        if next_item.is_none() && !sealed {
            next_item = subscription.recv_timeout(Duration::from_millis(25));
        }
        while let Some(item) = next_item {
            match item {
                SurfaceSubscriptionItem::Batch { batch } => {
                    terminal_seen |= batch.events.as_slice().iter().any(|envelope| {
                        matches!(
                            &envelope.event,
                            SurfaceEvent::Operation(OperationPatch::Terminal { record })
                                if &record.operation_id == operation_id
                        )
                    });
                    for envelope in batch.events.as_slice() {
                        if let SurfaceEvent::Interaction(
                            orca_runtime::surface::InteractionPatch::Requested { interaction },
                        ) = &envelope.event
                        {
                            if let Some(event) =
                                controller.register_surface_interaction(interaction)
                            {
                                let _ = event_tx.send(event);
                            }
                        }
                    }
                    match projection.project_typed_batch(&batch) {
                        Ok(events) => {
                            for event in events {
                                if matches!(event, TuiEvent::SessionCompleted { .. }) {
                                    projected_terminal_event = Some(event);
                                } else if defer_compacted_until_terminal
                                    && matches!(&event, TuiEvent::Compacted { .. })
                                {
                                    projected_compacted_event = Some(event);
                                } else {
                                    let _ = event_tx.send(event);
                                }
                            }
                        }
                        Err(error) => {
                            failure = Some(io::Error::other(format!(
                                "typed TUI projection failed: {error:?}"
                            )));
                            sealed = true;
                            break;
                        }
                    }
                }
                SurfaceSubscriptionItem::Gap { required } => {
                    failure = Some(io::Error::other(format!(
                        "typed TUI subscription gap requires {:?}",
                        required.reason
                    )));
                    sealed = true;
                    break;
                }
                SurfaceSubscriptionItem::Sealed { .. } => sealed = true,
            }
            if sealed {
                break;
            }
            next_item = subscription.try_recv();
        }
        if let Ok(result) = wait_rx.try_recv() {
            match result {
                Ok(WaitOperationTerminalResult::Terminal { value }) => {
                    if &value.operation_id == operation_id {
                        terminal_receipt = Some(value);
                    } else {
                        failure = Some(io::Error::other(
                            "typed TUI terminal waiter returned another operation",
                        ));
                        sealed = true;
                    }
                }
                Ok(other) => {
                    let message = terminal_wait_failure_message(&other);
                    failure = Some(
                        if matches!(
                            other,
                            WaitOperationTerminalResult::TerminalCommitFailure { .. }
                                | WaitOperationTerminalResult::TerminalProjectionFailure { .. }
                        ) {
                            io::Error::new(io::ErrorKind::Other, TerminalRecoveryRequired(message))
                        } else {
                            io::Error::other(message)
                        },
                    );
                    sealed = true;
                }
                Err(error) => {
                    failure = Some(io::Error::other(format!(
                        "typed TUI terminal wait failed: {error:?}"
                    )));
                    sealed = true;
                }
            }
        }
        if (!terminal_seen || terminal_receipt.is_none()) && !sealed {
            if controller.is_shutdown() {
                let _ = client.cancel_operation(SurfaceRequestId::new(), operation_id.clone());
            }
        }
    }
    if let Some(terminal) = terminal_receipt {
        controller.complete_surface(operation_id);
        controller.remember_surface_delivery_watermark(
            operation_id.clone(),
            projection.delivery_watermark(operation_id),
        );
        controller.remember_surface_terminal_delivery(operation_id.clone());
        if let Some(compacted) = projected_compacted_event.take() {
            let _ = event_tx.send(compacted);
        }
        let terminal_event =
            projected_terminal_event.unwrap_or_else(|| TuiEvent::SessionCompleted {
                status: terminal_status(terminal.terminal.clone()).to_string(),
            });
        let _ = event_tx.send(terminal_event);
        waiter_guard.join();
        return Ok(TuiHostedOperationOutcome::Turn {
            status: terminal_status(terminal.terminal).to_string(),
        });
    }
    if failure.is_none() && (sealed || !terminal_seen) {
        failure = Some(io::Error::other(
            "typed TUI surface closed before terminal reconciliation",
        ));
    }
    if let Some(error) = failure {
        let _ = client.cancel_operation(SurfaceRequestId::new(), operation_id.clone());
        waiter_guard.cancel_and_join();
        return Err(error);
    }
    waiter_guard.join();
    let terminal = terminal_receipt.expect("terminal receipt checked above");
    let status = terminal_status(terminal.terminal);
    Ok(TuiHostedOperationOutcome::Turn {
        status: status.to_string(),
    })
}

fn spawn_background_presentation(
    surface: &RuntimeSurfaceHandle,
    operation_id: SurfaceOperationId,
    controller: &TuiSurfaceTaskControl,
    event_tx: mpsc::Sender<TuiEvent>,
) -> io::Result<()> {
    let surface = surface.clone();
    let presentation_controller = controller.clone();
    controller.spawn_surface_presentation(
        operation_id.clone(),
        "orca-tui-background-presentation",
        move |cancellation| {
            if let Err(error) = monitor_background_presentation(
                &surface,
                &operation_id,
                &presentation_controller,
                &event_tx,
                &cancellation,
            ) {
                if !presentation_controller.is_shutdown() {
                    let _ = send_background_presentation_event(
                        &event_tx,
                        &presentation_controller,
                        &cancellation,
                        TuiEvent::Error(format!(
                            "typed TUI background presentation failed: {error}"
                        )),
                    );
                }
            }
        },
    )
}

fn monitor_background_presentation(
    surface: &RuntimeSurfaceHandle,
    operation_id: &SurfaceOperationId,
    controller: &TuiSurfaceTaskControl,
    event_tx: &mpsc::Sender<TuiEvent>,
    cancellation: &SurfacePresentationCancellation,
) -> io::Result<()> {
    let mut approval_notice_sent = false;
    let mut last_task = None;
    loop {
        if background_presentation_stopped(controller, cancellation) {
            return Ok(());
        }
        let attach_deadline = Instant::now() + Duration::from_secs(5);
        let attachment = loop {
            if background_presentation_stopped(controller, cancellation) {
                return Ok(());
            }
            match surface.attach_fresh(FreshAttachRequest {
                request_id: SurfaceRequestId::new(),
                role: SurfaceAttachmentRole::Tui,
                requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
                interaction_capabilities: BTreeSet::new(),
            }) {
                AttachResult::FreshAttached { attachment } => break attachment,
                AttachResult::Unavailable {
                    reason:
                        SurfaceUnavailableReason::CapacityExceeded
                        | SurfaceUnavailableReason::RuntimeUnavailable,
                } if Instant::now() < attach_deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                AttachResult::ThreadClosed { .. } => return Ok(()),
                AttachResult::Denied { reason } => {
                    return Err(io::Error::other(format!(
                        "background presentation attachment denied: {reason:?}"
                    )));
                }
                AttachResult::Unavailable { reason } => {
                    return Err(io::Error::other(format!(
                        "background presentation attachment unavailable: {reason:?}"
                    )));
                }
                AttachResult::CursorAttached { .. }
                | AttachResult::SnapshotRequired { .. }
                | AttachResult::InvalidCursor { .. } => {
                    return Err(io::Error::other(
                        "background presentation fresh attachment returned an invalid result",
                    ));
                }
            }
        };
        let mut subscription = match surface.claim_subscription(&attachment.subscription) {
            Some(subscription) => subscription,
            None => {
                detach(surface, &attachment.client);
                return Err(io::Error::other(
                    "background presentation subscription unavailable",
                ));
            }
        };
        let mut projection =
            TuiSurfaceProjection::from_surface_snapshot(&attachment.baseline.snapshot);
        let baseline_projection =
            SurfaceProjectionState::from_surface_snapshot(&attachment.baseline.snapshot);
        if !send_background_presentation_event(
            event_tx,
            controller,
            cancellation,
            TuiEvent::SurfaceProjectionSynced(Box::new(baseline_projection)),
        ) {
            detach(surface, &attachment.client);
            return Ok(());
        }
        let snapshot_task = projection.background_task_summary_for_operation(operation_id);
        if last_task.as_ref() != Some(&snapshot_task) {
            if !publish_background_approval_notice(
                snapshot_task.clone(),
                controller,
                cancellation,
                event_tx,
                &mut approval_notice_sent,
            ) {
                detach(surface, &attachment.client);
                return Ok(());
            }
            last_task = Some(snapshot_task);
        }
        if snapshot_operation_is_terminal(&attachment.baseline.snapshot, operation_id) {
            detach(surface, &attachment.client);
            return Ok(());
        }

        let reattach = loop {
            if background_presentation_stopped(controller, cancellation) {
                break false;
            }
            let Some(item) = subscription.recv_timeout(Duration::from_millis(25)) else {
                continue;
            };
            match item {
                SurfaceSubscriptionItem::Batch { batch } => {
                    let terminal = batch.events.as_slice().iter().any(|envelope| {
                        matches!(
                            &envelope.event,
                            SurfaceEvent::Operation(OperationPatch::Terminal { record })
                                if &record.operation_id == operation_id
                        )
                    });
                    let projection_events =
                        projection.project_typed_batch(&batch).map_err(|error| {
                            io::Error::other(format!(
                                "background presentation projection failed: {error:?}"
                            ))
                        })?;
                    let projection_sync = projection_events
                        .into_iter()
                        .find(|event| matches!(event, TuiEvent::SurfaceProjectionSynced(_)));
                    let current_task =
                        projection.background_task_summary_for_operation(operation_id);
                    if let Some(event) = projection_sync
                        && !send_background_presentation_event(
                            event_tx,
                            controller,
                            cancellation,
                            event,
                        )
                    {
                        break false;
                    }
                    if last_task.as_ref() != Some(&current_task) {
                        if !publish_background_approval_notice(
                            current_task.clone(),
                            controller,
                            cancellation,
                            event_tx,
                            &mut approval_notice_sent,
                        ) {
                            break false;
                        }
                        last_task = Some(current_task);
                    }
                    if terminal {
                        break false;
                    }
                }
                SurfaceSubscriptionItem::Gap { .. } => break true,
                SurfaceSubscriptionItem::Sealed { .. } => break false,
            }
        };
        detach(surface, &attachment.client);
        if !reattach {
            return Ok(());
        }
    }
}

fn publish_background_approval_notice(
    task: Option<BackgroundTaskSummary>,
    controller: &TuiSurfaceTaskControl,
    cancellation: &SurfacePresentationCancellation,
    event_tx: &mpsc::Sender<TuiEvent>,
    approval_notice_sent: &mut bool,
) -> bool {
    if *approval_notice_sent {
        return true;
    }
    let Some(task) = task.filter(|task| {
        task.task_type == TaskType::MainSession
            && task.is_backgrounded
            && task.status == TaskStatus::ApprovalRequired
    }) else {
        return true;
    };
    let notice = match task.tool.as_deref() {
        Some(tool) => {
            format!("Background session needs approval for {tool} before it can continue.")
        }
        None => "Background session needs approval before it can continue.".to_string(),
    };
    *approval_notice_sent = true;
    send_background_presentation_event(event_tx, controller, cancellation, TuiEvent::Notice(notice))
}

fn send_background_presentation_event(
    event_tx: &mpsc::Sender<TuiEvent>,
    controller: &TuiSurfaceTaskControl,
    cancellation: &SurfacePresentationCancellation,
    mut event: TuiEvent,
) -> bool {
    loop {
        if background_presentation_stopped(controller, cancellation) {
            return false;
        }
        match event_tx.send_timeout(event, Duration::from_millis(25)) {
            Ok(()) => return true,
            Err(mpsc::SendTimeoutError::Timeout(returned)) => event = returned,
            Err(mpsc::SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

fn background_presentation_stopped(
    controller: &TuiSurfaceTaskControl,
    cancellation: &SurfacePresentationCancellation,
) -> bool {
    cancellation.is_cancelled() || controller.is_shutdown()
}

fn snapshot_operation_is_terminal(
    snapshot: &SurfaceSnapshot,
    operation_id: &SurfaceOperationId,
) -> bool {
    snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .any(|operation| &operation.operation_id == operation_id && operation.terminal.is_some())
}

fn terminal_wait_failure_message(result: &WaitOperationTerminalResult) -> &'static str {
    match result {
        WaitOperationTerminalResult::Terminal { .. } => {
            "typed TUI terminal waiter returned terminal"
        }
        WaitOperationTerminalResult::TerminalCommitFailure { .. } => {
            "typed TUI terminal commit requires recovery"
        }
        WaitOperationTerminalResult::TerminalProjectionFailure { .. } => {
            "typed TUI terminal projection requires recovery"
        }
        WaitOperationTerminalResult::UnknownOperation { .. } => {
            "typed TUI terminal waiter lost operation"
        }
        WaitOperationTerminalResult::WrongThread { .. } => {
            "typed TUI terminal waiter used wrong thread"
        }
        WaitOperationTerminalResult::WaitCancelled { .. } => {
            "typed TUI terminal waiter was cancelled"
        }
    }
}

fn terminal_status(terminal: OperationTerminal) -> &'static str {
    match terminal {
        OperationTerminal::Succeeded { .. } => "success",
        OperationTerminal::Cancelled { .. } | OperationTerminal::Shutdown { .. } => "cancelled",
        OperationTerminal::BudgetExhausted { .. } => "budget_exhausted",
        OperationTerminal::NotAdmitted { .. } => "not_admitted",
        OperationTerminal::Failed { .. }
        | OperationTerminal::Panicked { .. }
        | OperationTerminal::JoinFailed { .. }
        | OperationTerminal::AbortedByRuntimeRestart { .. } => "failed",
    }
}

fn detach(surface: &RuntimeSurfaceHandle, client: &RuntimeSurfaceClientHandle) {
    let _ = surface.detach(
        client,
        orca_runtime::surface::DetachRequest {
            request_id: SurfaceRequestId::new(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_projection_cursor_requires_committed_identity_and_sequence() {
        let committed = crate::surface_projection::test_surface_cursor(2);
        let equal = committed.clone();
        let newer = crate::surface_projection::test_surface_cursor(3);
        let older = crate::surface_projection::test_surface_cursor(1);
        assert!(goal_projection_cursor_covers_commit(&equal, &committed));
        assert!(goal_projection_cursor_covers_commit(&newer, &committed));
        assert!(!goal_projection_cursor_covers_commit(&older, &committed));

        let mut different_thread = newer.clone();
        let mut thread_bytes = [3; 16];
        thread_bytes[6] = 0x73;
        thread_bytes[8] = 0x83;
        different_thread.thread_id =
            orca_runtime::surface::SurfaceThreadId::try_from_bytes(thread_bytes)
                .expect("different test thread id");
        assert!(!goal_projection_cursor_covers_commit(
            &different_thread,
            &committed
        ));

        let mut different_incarnation = newer;
        let mut incarnation_bytes = [4; 16];
        incarnation_bytes[6] = 0x74;
        incarnation_bytes[8] = 0x84;
        different_incarnation.incarnation =
            orca_runtime::surface::SurfaceIncarnation::try_from_bytes(incarnation_bytes)
                .expect("different test surface incarnation");
        assert!(!goal_projection_cursor_covers_commit(
            &different_incarnation,
            &committed
        ));
    }

    #[test]
    fn background_presentation_shutdown_does_not_deadlock_on_a_full_tui_mailbox() {
        let controller = TuiSurfaceTaskControl::new();
        let monitor_controller = controller.clone();
        let (event_tx, _event_rx) = mpsc::bounded(1);
        event_tx
            .send(TuiEvent::Notice("fill TUI mailbox".to_string()))
            .expect("fill TUI mailbox");
        let mut operation_bytes = [31; 16];
        operation_bytes[6] = 0x7f;
        operation_bytes[8] = 0x9f;
        let operation_id =
            SurfaceOperationId::try_from_bytes(operation_bytes).expect("operation id");
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let monitor_exited = std::sync::Arc::clone(&exited);
        controller
            .spawn_surface_presentation(
                operation_id,
                "full-mailbox-presentation",
                move |cancellation| {
                    started_tx.send(()).expect("monitor started");
                    assert!(!send_background_presentation_event(
                        &event_tx,
                        &monitor_controller,
                        &cancellation,
                        TuiEvent::Notice("blocked presentation".to_string()),
                    ));
                    monitor_exited.store(true, std::sync::atomic::Ordering::Release);
                },
            )
            .expect("spawn monitor");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("monitor entered send loop");

        controller.shutdown();

        assert!(exited.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn workflow_args_preserve_json_scalar_types() {
        let args = parse_workflow_args(Some(
            r#"{"label":"alpha","count":2,"enabled":true,"empty":null}"#,
        ))
        .expect("typed workflow args");
        let decoded = args
            .into_iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    serde_json::from_str::<serde_json::Value>(value.as_str())
                        .expect("canonical JSON value"),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(decoded["label"], serde_json::json!("alpha"));
        assert_eq!(decoded["count"], serde_json::json!(2));
        assert_eq!(decoded["enabled"], serde_json::json!(true));
        assert_eq!(decoded["empty"], serde_json::Value::Null);
    }

    #[test]
    fn workflow_key_value_args_keep_strings_and_parse_json_literals() {
        let args = parse_workflow_args(Some("label=alpha count=2 enabled=true empty=null"))
            .expect("key value workflow args");
        let encoded = args
            .into_iter()
            .map(|(name, value)| (name.as_str().to_string(), value.as_str().to_string()))
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(encoded["label"], r#""alpha""#);
        assert_eq!(encoded["count"], "2");
        assert_eq!(encoded["enabled"], "true");
        assert_eq!(encoded["empty"], "null");
    }

    #[test]
    fn manual_compaction_failed_terminal_is_not_reported_as_success() {
        let error = match manual_compaction_terminal_outcome(TuiHostedOperationOutcome::Turn {
            status: "failed".to_string(),
        }) {
            Ok(_) => panic!("failed manual compaction must surface to the TUI action"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("status failed"));
        assert!(matches!(
            manual_compaction_terminal_outcome(TuiHostedOperationOutcome::Turn {
                status: "cancelled".to_string(),
            })
            .expect("cancelled manual compaction remains a settled user outcome"),
            TuiHostedOperationOutcome::ManualCompaction
        ));
    }
    use orca_core::config::HistoryMode;
    use orca_runtime::runtime_host::{RuntimeHost, RuntimeThreadHandle};
    use std::time::Instant;

    use crate::types::TuiTaskLifecycle;

    fn run_through_dispatch(
        thread: &RuntimeThreadHandle,
        request: HostedTurnRequest,
        config: RunConfig,
        controller: &TuiSurfaceTaskControl,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        let typed_thread = thread.typed_surface();
        run(&typed_thread, request, config, controller, event_tx)
    }

    #[test]
    fn typed_ordinary_turn_projects_terminal_and_assistant_output() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI turn")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();

        let outcome = run_through_dispatch(
            &thread,
            HostedTurnRequest::new("hello from typed TUI"),
            config,
            &controller,
            &event_tx,
        )
        .expect("typed operation");
        let events = event_rx.try_iter().collect::<Vec<_>>();

        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            TuiEvent::MessageDelta(_) | TuiEvent::AssistantResponseCompleted(_, _)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            TuiEvent::TurnStarted {
                task: Some(TuiTaskLifecycle { status, .. }),
                ..
            } if status == "running"
        )));
        assert!(events.iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));
        let projection = events
            .iter()
            .rev()
            .find_map(|event| match event {
                TuiEvent::SurfaceProjectionSynced(projection) => Some(projection.as_ref()),
                _ => None,
            })
            .expect("typed projection must finish each batch with a reducer snapshot");
        assert_eq!(projection.title, "typed TUI turn");
        assert!(projection.usage_revision > 0);
        assert!(projection.foreground_operation_id.is_none());
        assert!(controller.current_id().is_none());
        assert!(!controller.has_surface_active());

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_manual_compaction_projects_durable_lifecycle() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config, "typed TUI manual compaction")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();

        let outcome = manual_compact(&thread.typed_surface(), &controller, &event_tx)
            .expect("typed manual compaction");
        let events = event_rx.try_iter().collect::<Vec<_>>();
        let started = events
            .iter()
            .position(|event| matches!(event, TuiEvent::CompactionStarted))
            .expect("compaction started");
        let completed = events
            .iter()
            .position(|event| matches!(event, TuiEvent::Compacted { .. }))
            .expect("compaction completed");
        let projection = events
            .iter()
            .rposition(|event| matches!(event, TuiEvent::SurfaceProjectionSynced(_)))
            .expect("compaction projection snapshot");
        let terminal = events
            .iter()
            .position(
                |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success"),
            )
            .expect("compaction terminal");

        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::ManualCompaction
        ));
        assert!(started < projection);
        assert!(projection < completed);
        assert!(completed < terminal);
        assert!(controller.current_id().is_none());
        assert!(!controller.has_surface_active());

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_interrupt_uses_surface_cancel() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI cancellation")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::bounded(1);
        let run_thread = thread.clone();
        let run_controller = controller.clone();
        let run_config = config;
        let worker = std::thread::spawn(move || {
            let result = run_through_dispatch(
                &run_thread,
                HostedTurnRequest::new("mock_stream_delay_ms 5000"),
                run_config,
                &run_controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !controller.has_surface_active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.has_surface_active());

        let _ = controller.interrupt_current();
        let outcome = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("typed cancellation terminal")
            .expect("typed cancellation outcome");
        worker.join().expect("typed TUI worker");

        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "cancelled"
        ));
        assert!(!controller.has_surface_active());

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_backgrounds_only_after_durable_surface_handoff() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI background handoff")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::bounded(1);
        let run_thread = thread.clone();
        let run_controller = controller.clone();
        let next_config = config.clone();
        let worker = std::thread::spawn(move || {
            let result = run_through_dispatch(
                &run_thread,
                HostedTurnRequest::new("mock_stream_delay_ms 1000")
                    .with_task_description("typed background turn"),
                config,
                &run_controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !controller.has_surface_active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.has_surface_active());
        let first_delta_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if event_rx.try_iter().any(|event| {
                matches!(
                    event,
                    TuiEvent::MessageDelta(ref text) if text == "Mock slow stream started."
                )
            }) {
                break;
            }
            assert!(
                Instant::now() < first_delta_deadline,
                "first provider delta must be displayed before backgrounding"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.request_background_current());

        let outcome = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("typed background handoff")
            .expect("typed background outcome");
        worker.join().expect("typed TUI worker");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "backgrounded"
        ));
        assert!(!controller.has_surface_active());

        let snapshot = read_snapshot(&thread.typed_surface()).expect("typed snapshot");
        assert_eq!(snapshot.background_operations.len(), 1);
        let background = &snapshot.background_operations[0];
        let background_operation_id = background.operation_id.clone();
        assert!(
            !controller
                .surface_delivery_watermark(&background_operation_id)
                .is_empty(),
            "background detach must retain the exact displayed stream offsets"
        );
        let task_id = background
            .task_id
            .as_ref()
            .expect("background handoff owns a task")
            .clone();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id == task_id)
            .expect("background task is durable");
        assert_eq!(
            task.task_type,
            orca_runtime::surface::SurfaceTaskType::MainSession
        );
        assert!(task.backgrounded);
        assert!(
            task.background_fence.as_ref() == Some(&background.fence),
            "task and operation must share the exact background owner"
        );

        let (next_event_tx, _next_event_rx) = mpsc::unbounded();
        let next_outcome = run_through_dispatch(
            &thread,
            HostedTurnRequest::new("mock_stream_delay_ms 1500"),
            next_config,
            &controller,
            &next_event_tx,
        )
        .expect("next typed foreground turn");
        assert!(matches!(
            next_outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        let background_monitor_events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            background_monitor_events.iter().all(|event| matches!(
                event,
                TuiEvent::Notice(_) | TuiEvent::SurfaceProjectionSynced(_)
            )),
            "background observer must not project a later foreground operation: \
             {background_monitor_events:?}"
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = read_snapshot(&thread.typed_surface()).expect("terminal snapshot");
            let task_completed = snapshot.tasks.iter().any(|task| {
                task.task_id == task_id
                    && task.status == orca_runtime::surface::SurfaceTaskStatus::Completed
            });
            let operation_terminal = snapshot.operation_history.iter().any(|operation| {
                operation.operation_id == background_operation_id && operation.terminal.is_some()
            });
            if task_completed && operation_terminal {
                assert!(
                    snapshot
                        .background_operations
                        .iter()
                        .all(|operation| operation.operation_id != background_operation_id)
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "background provider must durably terminalize"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn sessionless_foreground_requires_runtime_surface() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Disabled;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config, "sessionless foreground")
            .expect("runtime thread");
        let registry = thread.task_registry();
        let task = registry.create_main_session("legacy background task".to_string());
        registry.mark_running(&task.id).expect("task running");
        registry
            .mark_backgrounded(&task.id)
            .expect("task backgrounded");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, _event_rx) = mpsc::unbounded();

        let error = foreground_task(&thread.typed_surface(), &task.id, &controller, &event_tx)
            .expect_err("sessionless task must not bypass the typed surface");
        assert!(error.contains("attachment unavailable"), "{error}");

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
    }

    #[test]
    fn foreground_after_background_before_first_delta_hydrates_typed_output_and_terminal() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "foreground completed background output")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::bounded(1);
        let run_thread = thread.clone();
        let run_controller = controller.clone();
        let worker = std::thread::spawn(move || {
            let result = run_through_dispatch(
                &run_thread,
                HostedTurnRequest::new("mock_stream_delay_ms 250")
                    .with_task_description("completed background output"),
                config,
                &run_controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        let turn_started_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if event_rx
                .try_iter()
                .any(|event| matches!(event, TuiEvent::TurnStarted { .. }))
            {
                break;
            }
            assert!(
                Instant::now() < turn_started_deadline,
                "turn must start before backgrounding"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.request_background_current());
        let backgrounded = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("background handoff")
            .expect("background outcome");
        worker.join().expect("background worker");
        assert!(matches!(
            backgrounded,
            TuiHostedOperationOutcome::Turn { status } if status == "backgrounded"
        ));
        let background_snapshot =
            read_snapshot(&thread.typed_surface()).expect("background snapshot");
        let background = background_snapshot
            .background_operations
            .first()
            .expect("background operation");
        let operation_id = background.operation_id.clone();
        let task_id = background
            .task_id
            .as_ref()
            .expect("background task")
            .as_str()
            .to_string();

        let terminal_deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = read_snapshot(&thread.typed_surface()).expect("terminal snapshot");
            if snapshot.operation_history.iter().any(|operation| {
                operation.operation_id == operation_id && operation.terminal.is_some()
            }) {
                break;
            }
            assert!(
                Instant::now() < terminal_deadline,
                "background provider must terminalize before foreground attach"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let (foreground_tx, foreground_rx) = mpsc::unbounded();
        foreground_task(
            &thread.typed_surface(),
            &task_id,
            &controller,
            &foreground_tx,
        )
        .expect("terminal foreground attach");
        let foreground_events = foreground_rx.try_iter().collect::<Vec<_>>();
        assert!(foreground_events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::MessageDelta(text)
                    if text.contains("Mock slow stream completed.")
            )
        }));
        assert!(foreground_events.iter().any(|event| {
            matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        }));
        assert!(
            !controller
                .surface_delivery_watermark(&operation_id)
                .is_empty()
        );
        let restarted_controller = TuiSurfaceTaskControl::isolated_for_test();
        let (restart_tx, restart_rx) = mpsc::unbounded();
        foreground_task(
            &thread.typed_surface(),
            &task_id,
            &restarted_controller,
            &restart_tx,
        )
        .expect("restart-local terminal attach hydrates typed output");
        let restart_events = restart_rx.try_iter().collect::<Vec<_>>();
        assert!(restart_events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::MessageDelta(text)
                    if text.contains("Mock slow stream completed.")
            )
        }));
        assert!(restart_events.iter().any(|event| {
            matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        }));

        let (duplicate_tx, duplicate_rx) = mpsc::unbounded();
        let duplicate = foreground_task(
            &thread.typed_surface(),
            &task_id,
            &restarted_controller,
            &duplicate_tx,
        )
        .expect_err("same controller rejects a duplicate terminal delivery");
        assert!(duplicate.contains("already delivered"));
        assert!(duplicate_rx.try_recv().is_err());

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn foregrounded_provider_can_be_backgrounded_again_without_changing_execution_owner() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "foreground and re-background")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::bounded(1);
        let run_thread = thread.clone();
        let run_controller = controller.clone();
        let worker = std::thread::spawn(move || {
            let result = run_through_dispatch(
                &run_thread,
                HostedTurnRequest::new("mock_stream_delay_ms 3000")
                    .with_task_description("re-background provider"),
                config,
                &run_controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        let first_delta_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if event_rx.try_iter().any(|event| {
                matches!(
                    event,
                    TuiEvent::MessageDelta(ref text) if text == "Mock slow stream started."
                )
            }) {
                break;
            }
            assert!(
                Instant::now() < first_delta_deadline,
                "first delta must be displayed before backgrounding"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.request_background_current());
        let backgrounded = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first background handoff")
            .expect("first background outcome");
        worker.join().expect("first background worker");
        assert!(matches!(
            backgrounded,
            TuiHostedOperationOutcome::Turn { status } if status == "backgrounded"
        ));
        let snapshot = read_snapshot(&thread.typed_surface()).expect("background snapshot");
        let task_id = snapshot.background_operations[0]
            .task_id
            .as_ref()
            .expect("provider task")
            .as_str()
            .to_string();
        let surface = thread.typed_surface().surface();
        let direct_foreground = match surface.attach_fresh(FreshAttachRequest {
            request_id: SurfaceRequestId::new(),
            role: SurfaceAttachmentRole::Tui,
            requested_capabilities: BTreeSet::from([
                SurfaceCapability::ReadSnapshot,
                SurfaceCapability::ManageTask,
                SurfaceCapability::ControlBoundOperation,
            ]),
            interaction_capabilities: BTreeSet::new(),
        }) {
            AttachResult::FreshAttached { attachment } => attachment,
            _ => panic!("direct foreground attachment failed"),
        };
        let task = direct_foreground
            .baseline
            .snapshot
            .tasks
            .iter()
            .find(|task| task.task_id.as_str() == task_id)
            .expect("direct foreground task");
        assert!(matches!(
            direct_foreground.client.task_control(
                SurfaceRequestId::new(),
                TaskControlAction::Foreground {
                    fence: SurfaceTaskFence {
                        task_id: task.task_id.clone(),
                        task_revision: task.revision,
                        background_owner: task.background_fence.clone(),
                    },
                },
            ),
            Ok(MutationReply::Committed { .. })
        ));
        detach(&surface, &direct_foreground.client);

        let (foreground_tx, foreground_rx) = mpsc::bounded(1);
        let foreground_thread = thread.typed_surface();
        let foreground_controller = controller.clone();
        let foreground_task_id = task_id.clone();
        let (foreground_event_tx, _foreground_event_rx) = mpsc::unbounded();
        let foreground_worker = std::thread::spawn(move || {
            let result = foreground_task(
                &foreground_thread,
                &foreground_task_id,
                &foreground_controller,
                &foreground_event_tx,
            );
            let _ = foreground_tx.send(result);
        });
        let active_deadline = Instant::now() + Duration::from_secs(2);
        while !controller.has_surface_active() && Instant::now() < active_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            controller.has_surface_active(),
            "foreground attach must install the TUI controller"
        );
        assert!(controller.request_background_current());
        let projection = match foreground_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result.expect("second background handoff"),
            Err(error) => {
                let _ = controller.interrupt_current();
                let _ = foreground_rx.recv_timeout(Duration::from_secs(3));
                foreground_worker
                    .join()
                    .expect("foreground worker after cancellation");
                panic!("foregrounded provider did not re-background: {error}");
            }
        };
        foreground_worker
            .join()
            .expect("foreground re-background worker");
        assert!(
            projection
                .workflow_tasks
                .iter()
                .any(|task| task.id == task_id && task.is_backgrounded)
        );
        let rebackgrounded =
            read_snapshot(&thread.typed_surface()).expect("re-backgrounded snapshot");
        assert_eq!(rebackgrounded.background_operations.len(), 1);
        assert!(
            rebackgrounded
                .tasks
                .iter()
                .any(|task| task.task_id.as_str() == task_id && task.backgrounded)
        );

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_background_approval_suspends_original_operation_and_requests_bound_interaction() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed background approval")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::bounded(1);
        let run_thread = thread.clone();
        let run_controller = controller.clone();
        let worker = std::thread::spawn(move || {
            let result = run_through_dispatch(
                &run_thread,
                HostedTurnRequest::new("mock_stream_tool_delay_ms 250 task_list")
                    .with_task_description("typed approval background"),
                config,
                &run_controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !controller.has_surface_active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.has_surface_active());
        assert!(controller.request_background_current());
        let outcome = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("typed background handoff")
            .expect("typed background outcome");
        worker.join().expect("typed TUI worker");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "backgrounded"
        ));

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = read_snapshot(&thread.typed_surface()).expect("typed snapshot");
            let observed = snapshot
                .tasks
                .iter()
                .map(|task| {
                    format!(
                        "{}:{:?}:{}",
                        task.task_id.as_str(),
                        task.status,
                        task.description.as_str()
                    )
                })
                .collect::<Vec<_>>();
            let task = snapshot.tasks.iter().find(|task| {
                task.task_type == orca_runtime::surface::SurfaceTaskType::MainSession
                    && task.status == orca_runtime::surface::SurfaceTaskStatus::ApprovalRequired
            });
            if let Some(task) = task {
                let operation_id = task
                    .parent_operation
                    .as_ref()
                    .expect("approval task keeps its operation identity");
                let background = snapshot
                    .background_operations
                    .iter()
                    .find(|operation| &operation.operation_id == operation_id)
                    .expect("approval operation remains durably suspended in background");
                let pending_tool = snapshot
                    .tools
                    .iter()
                    .find(|tool| tool.request.tool_call_id.as_str() == "mock-tool-1")
                    .expect("approval task keeps its typed provider tool request");
                assert_eq!(pending_tool.request.name.as_str(), "task_list");
                assert_eq!(
                    pending_tool.request.action,
                    orca_runtime::surface::SurfaceToolAction::Read
                );
                assert_eq!(pending_tool.request.raw_arguments.as_str(), "{}");
                assert!(pending_tool.result.is_none());
                let operation = snapshot
                    .operation_history
                    .iter()
                    .find(|operation| &operation.operation_id == operation_id)
                    .expect("background operation keeps its durable operation record");
                assert!(operation.finalization.is_none());
                assert!(operation.terminal.is_none());
                let interaction = snapshot
                    .interactions
                    .iter()
                    .find(|interaction| {
                        interaction.fence.operation_id == *operation_id
                            && interaction.kind
                                == orca_runtime::surface::SurfaceInteractionKind::BackgroundApproval
                    })
                    .expect("approval task exposes a typed background approval");
                let orca_runtime::surface::SurfaceInteractionRequest::BackgroundApproval {
                    task: task_fence,
                    tool,
                    ..
                } = &interaction.request
                else {
                    panic!("background approval uses the exact typed request");
                };
                assert_eq!(task_fence.task_id, task.task_id);
                assert!(task_fence.background_owner.as_ref() == Some(&background.fence));
                assert_eq!(background.fence.operation_fence, interaction.fence);
                assert_eq!(tool, &pending_tool.request);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "approval-required task and suspended interaction must converge; observed {observed:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_workflow_stop_routes_through_operation_cancel() {
        if !orca_runtime::workflow::host::WorkflowHost::node_available() {
            return;
        }
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let workflow_dir = cwd.path().join(".orca").join("workflows");
        std::fs::create_dir_all(&workflow_dir).expect("workflow directory");
        std::fs::write(
            workflow_dir.join("tui-stop.js"),
            "export const meta = { name: 'tui-stop', description: 'tui stop', phases: ['main'] };\nexport default await phase('main', async () => agent('mock_stream_delay_ms 30000'));",
        )
        .expect("workflow source");
        orca_core::config::folder_trust::set_trust_with_config_dir(
            cwd.path(),
            home.path(),
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .expect("trusted workflow workspace");
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(cwd.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        config.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config, "typed TUI workflow stop")
            .expect("runtime thread");
        let actions = crate::surface_actions::TuiSurfaceActions::new(thread.typed_surface());
        let (event_tx, _event_rx) = mpsc::unbounded();

        actions
            .launch_workflow("tui-stop", None, &event_tx)
            .expect("typed workflow launch");
        let deadline = Instant::now() + Duration::from_secs(5);
        let (task_id, operation_id, workflow_run_id) = loop {
            let snapshot = actions.read_snapshot().expect("workflow snapshot");
            if let Some(task) = snapshot
                .tasks
                .iter()
                .find(|task| task.workflow_run_id.is_some() && task.background_fence.is_some())
            {
                break (
                    task.task_id.as_str().to_string(),
                    task.background_fence
                        .as_ref()
                        .expect("background fence")
                        .operation_fence
                        .operation_id
                        .clone(),
                    task.workflow_run_id.clone().expect("workflow run"),
                );
            }
            assert!(
                Instant::now() < deadline,
                "typed workflow never became background-owned"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let projection = stop_task(&thread.typed_surface(), &task_id, &controller, &event_tx)
            .expect("typed workflow stop");
        assert!(
            projection.workflow_tasks.iter().any(|task| {
                task.id == task_id
                    && matches!(
                        task.status,
                        orca_core::task_types::TaskStatus::Stopping
                            | orca_core::task_types::TaskStatus::Stopped
                            | orca_core::task_types::TaskStatus::Cancelled
                    )
            }),
            "TUI stop result must come from the typed task projection"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = actions.read_snapshot().expect("terminal workflow snapshot");
            let cancelled = snapshot.operation_history.iter().any(|operation| {
                operation.operation_id == operation_id
                    && operation.terminal.as_ref().is_some_and(|record| {
                        matches!(record.terminal, OperationTerminal::Cancelled { .. })
                    })
            });
            let workflow_stopped = snapshot.workflows.iter().any(|workflow| {
                workflow.workflow_run_id == workflow_run_id
                    && matches!(
                        workflow.status,
                        orca_runtime::surface::SurfaceWorkflowStatus::Stopped
                            | orca_runtime::surface::SurfaceWorkflowStatus::Cancelled
                    )
            });
            if cancelled && workflow_stopped {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "typed workflow stop never reached cancelled terminal"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_late_interrupt_does_not_cancel_next_turn() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed late interrupt")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (first_event_tx, _first_event_rx) = mpsc::unbounded();
        let first = run_through_dispatch(
            &thread,
            HostedTurnRequest::new("first completed turn"),
            config.clone(),
            &controller,
            &first_event_tx,
        )
        .expect("first typed turn");
        assert!(matches!(
            first,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));

        let _ = controller.interrupt_current();

        let (second_event_tx, second_event_rx) = mpsc::unbounded();
        let second = run_through_dispatch(
            &thread,
            HostedTurnRequest::new("second turn after late interrupt"),
            config,
            &controller,
            &second_event_tx,
        )
        .expect("second typed turn");
        assert!(matches!(
            second,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(second_event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_failed_prearmed_activation_does_not_cancel_next_turn() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed prearmed activation failure")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        assert!(controller.begin_surface_activation().expect("prearm"));

        let mut mismatched = config.clone();
        mismatched.approval_mode = orca_core::approval_types::ApprovalMode::AutoEdit;
        let (failed_event_tx, _failed_event_rx) = mpsc::unbounded();
        let typed_thread = thread.typed_surface();
        let error = match run_typed_thread(
            &typed_thread,
            HostedTurnRequest::new("must fail before reservation"),
            mismatched,
            &controller,
            &failed_event_tx,
        ) {
            Ok(_) => panic!("mismatched settings must reject the activation"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("config differs"));

        let _ = controller.interrupt_current();

        let (event_tx, event_rx) = mpsc::unbounded();
        let typed_thread = thread.typed_surface();
        let outcome = run_typed_thread(
            &typed_thread,
            HostedTurnRequest::new("turn after failed activation"),
            config,
            &controller,
            &event_tx,
        )
        .expect("next typed turn");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_routes_tool_approval_through_runtime_surface() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI approval")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();
        let worker_controller = controller.clone();
        let worker_thread = thread.clone();
        let worker_config = config.clone();
        let worker_event_tx = event_tx.clone();
        let worker = std::thread::spawn(move || {
            run_through_dispatch(
                &worker_thread,
                HostedTurnRequest::new("bash printf canonical-approval"),
                worker_config,
                &worker_controller,
                &worker_event_tx,
            )
        });
        let key = loop {
            match event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("approval event")
            {
                TuiEvent::ApprovalNeeded { key, .. } => break key,
                _ => {}
            }
        };
        assert!(
            controller
                .respond_surface_interaction(
                    &key,
                    &crate::types::TuiInteractionResponse::Approval(true)
                )
                .expect("typed approval response")
        );
        let outcome = worker
            .join()
            .expect("typed approval worker")
            .expect("typed approval");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_routes_permission_through_runtime_surface() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI permission")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();
        let worker_controller = controller.clone();
        let worker_thread = thread.clone();
        let worker_config = config.clone();
        let worker_event_tx = event_tx.clone();
        let worker = std::thread::spawn(move || {
            run_through_dispatch(
                &worker_thread,
                HostedTurnRequest::new("request_network_permissions_then_done example.com"),
                worker_config,
                &worker_controller,
                &worker_event_tx,
            )
        });
        let key = loop {
            match event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("permission event")
            {
                TuiEvent::PermissionApprovalNeeded { key, .. } => break key,
                _ => {}
            }
        };
        assert!(
            controller
                .respond_surface_interaction(
                    &key,
                    &crate::types::TuiInteractionResponse::Permission(true)
                )
                .expect("typed permission response")
        );
        let outcome = worker
            .join()
            .expect("typed permission worker")
            .expect("typed permission");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_routes_user_input_through_runtime_surface() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed TUI user input")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();
        let worker_controller = controller.clone();
        let worker_thread = thread.clone();
        let worker_config = config.clone();
        let worker_event_tx = event_tx.clone();
        let worker = std::thread::spawn(move || {
            run_through_dispatch(
                &worker_thread,
                HostedTurnRequest::new("ask continue?"),
                worker_config,
                &worker_controller,
                &worker_event_tx,
            )
        });
        let key = loop {
            match event_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("user input event")
            {
                TuiEvent::UserInputRequested { key, .. } => break key,
                _ => {}
            }
        };
        assert!(
            controller
                .respond_surface_interaction(
                    &key,
                    &crate::types::TuiInteractionResponse::UserInput("yes".to_string()),
                )
                .expect("typed user input response")
        );
        let outcome = worker
            .join()
            .expect("typed user input worker")
            .expect("typed user input");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        thread.shutdown().expect("thread shutdown");
        host.shutdown().expect("host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_ordinary_turn_reloads_and_runs_after_runtime_restart() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;

        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed restart source")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, event_rx) = mpsc::unbounded();
        let first = run_through_dispatch(
            &thread,
            HostedTurnRequest::new("before runtime restart"),
            config.clone(),
            &controller,
            &event_tx,
        )
        .expect("first typed turn");
        assert!(matches!(
            first,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));
        let thread_id = thread.thread_id().to_string();
        thread.shutdown().expect("first thread shutdown");
        host.shutdown().expect("first host shutdown");

        let mut resumed_config = config;
        resumed_config.history_mode = HistoryMode::Resume(thread_id);
        let resumed_host = RuntimeHost::start().expect("resumed runtime host");
        let resumed_thread = resumed_host
            .start_thread(resumed_config.clone(), "typed restart resumed")
            .expect("resumed runtime thread");
        let resumed_controller = TuiSurfaceTaskControl::isolated_for_test();
        let (resumed_event_tx, resumed_event_rx) = mpsc::unbounded();
        let second = run_through_dispatch(
            &resumed_thread,
            HostedTurnRequest::new("after runtime restart"),
            resumed_config,
            &resumed_controller,
            &resumed_event_tx,
        )
        .expect("resumed typed turn");
        assert!(matches!(
            second,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        let resumed_events = resumed_event_rx.try_iter().collect::<Vec<_>>();
        assert!(resumed_events.iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));
        assert!(
            resumed_events
                .iter()
                .any(|event| matches!(event, TuiEvent::MessageDelta(text) if !text.is_empty()))
        );

        resumed_thread.shutdown().expect("resumed thread shutdown");
        resumed_host.shutdown().expect("resumed host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_background_owner_is_terminalized_before_restart_reopens_the_thread() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;

        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed background restart source")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::bounded(1);
        let run_thread = thread.clone();
        let run_controller = controller.clone();
        let run_config = config.clone();
        let worker = std::thread::spawn(move || {
            let result = run_through_dispatch(
                &run_thread,
                HostedTurnRequest::new("mock_stream_delay_ms 5000"),
                run_config,
                &run_controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !controller.has_surface_active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.has_surface_active());
        assert!(controller.request_background_current());
        let outcome = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("typed background handoff")
            .expect("typed background outcome");
        worker.join().expect("typed background worker");
        assert!(matches!(
            outcome,
            TuiHostedOperationOutcome::Turn { status } if status == "backgrounded"
        ));
        let before = read_snapshot(&thread.typed_surface()).expect("background snapshot");
        let operation_id = before.background_operations[0].operation_id.clone();
        let thread_id = thread.thread_id().to_string();

        host.shutdown()
            .expect("host shutdown settles typed background owner");

        let mut resumed_config = config;
        resumed_config.history_mode = HistoryMode::Resume(thread_id);
        let resumed_host = RuntimeHost::start().expect("resumed runtime host");
        let resumed_thread = resumed_host
            .start_thread(resumed_config.clone(), "typed background restart resumed")
            .expect("resumed runtime thread");
        let resumed_snapshot =
            read_snapshot(&resumed_thread.typed_surface()).expect("resumed snapshot");
        assert!(resumed_snapshot.background_operations.is_empty());
        let recovered = resumed_snapshot
            .operation_history
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .expect("background operation survives as terminal history");
        assert!(matches!(
            recovered.terminal.as_ref().map(|record| &record.terminal),
            Some(OperationTerminal::Shutdown { .. })
        ));

        let resumed_controller = TuiSurfaceTaskControl::isolated_for_test();
        let (resumed_event_tx, _resumed_event_rx) = mpsc::unbounded();
        let next = run_through_dispatch(
            &resumed_thread,
            HostedTurnRequest::new("after background restart"),
            resumed_config,
            &resumed_controller,
            &resumed_event_tx,
        )
        .expect("next turn after background restart");
        assert!(matches!(
            next,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));

        resumed_thread.shutdown().expect("resumed thread shutdown");
        resumed_host.shutdown().expect("resumed host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }

    #[test]
    fn typed_cancelled_turn_restarts_and_next_turn_commits() {
        let _guard = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        let mut config = crate::test_support::test_run_config();
        config.cwd = Some(home.path().to_path_buf());
        config.history_mode = HistoryMode::Record;

        let host = RuntimeHost::start().expect("runtime host");
        let thread = host
            .start_thread(config.clone(), "typed cancellation restart source")
            .expect("runtime thread");
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let (event_tx, _event_rx) = mpsc::unbounded();
        let (result_tx, result_rx) = mpsc::bounded(1);
        let run_thread = thread.clone();
        let run_controller = controller.clone();
        let run_config = config.clone();
        let worker = std::thread::spawn(move || {
            let result = run_through_dispatch(
                &run_thread,
                HostedTurnRequest::new("mock_stream_delay_ms 5000"),
                run_config,
                &run_controller,
                &event_tx,
            );
            let _ = result_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !controller.has_surface_active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(controller.has_surface_active());
        let _ = controller.interrupt_current();

        let cancelled = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancelled terminal")
            .expect("cancelled typed turn");
        worker.join().expect("cancelled worker");
        assert!(matches!(
            cancelled,
            TuiHostedOperationOutcome::Turn { status } if status == "cancelled"
        ));
        let thread_id = thread.thread_id().to_string();
        thread.shutdown().expect("source thread shutdown");
        host.shutdown().expect("source host shutdown");

        let mut resumed_config = config;
        resumed_config.history_mode = HistoryMode::Resume(thread_id);
        let resumed_host = RuntimeHost::start().expect("resumed runtime host");
        let resumed_thread = resumed_host
            .start_thread(resumed_config.clone(), "typed cancellation restart resumed")
            .expect("resumed runtime thread");
        let resumed_controller = TuiSurfaceTaskControl::isolated_for_test();
        let (resumed_event_tx, resumed_event_rx) = mpsc::unbounded();
        let resumed = run_through_dispatch(
            &resumed_thread,
            HostedTurnRequest::new("after cancellation restart"),
            resumed_config,
            &resumed_controller,
            &resumed_event_tx,
        )
        .expect("resumed typed turn");
        assert!(matches!(
            resumed,
            TuiHostedOperationOutcome::Turn { status } if status == "success"
        ));
        assert!(resumed_event_rx.try_iter().any(
            |event| matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        ));

        resumed_thread.shutdown().expect("resumed thread shutdown");
        resumed_host.shutdown().expect("resumed host shutdown");
        match previous {
            Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
            None => unsafe { std::env::remove_var("ORCA_HOME") },
        }
    }
}
