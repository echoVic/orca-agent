use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::Duration;

use orca_core::goal_runtime::{
    GoalGap, GoalId, GoalNextAction, GoalOuterTurnId, GoalRecord, GoalState, GoalTurnOrigin,
    GoalTurnStatus, GoalUpdateAck, GoalUpdateIntent, GoalUsage, GoalVerificationResult,
};
use orca_core::goal_types::ThreadGoal;
use orca_platform::fs::ExclusiveFileLock;
use sha2::{Digest, Sha256};

use crate::goal_store::{
    BeginGoalOuterTurnForSurfaceInput, BeginGoalRunInput, BeginOuterTurnInput,
    CreateGoalAndPrepareRunForSurfaceInput, CreateGoalInput, EditGoalAndPrepareRunForSurfaceInput,
    FinishGoalOuterTurnForSurfaceInput, FinishOuterTurnInput, GoalIntentRecord, GoalRecoveryRecord,
    GoalStore, GoalStoreError, GoalSurfaceMutationContext, GoalSurfaceMutationRecord,
    GoalSurfaceRowState, GoalSurfaceTokenBudgetUpdate, GoalSurfaceTurnProgress, GoalUsageEvent,
    PauseGoalForSurfaceInput, PauseQuiescentGoalForSurfaceInput, PrepareGoalRunForSurfaceInput,
    RecoverGoalRunForSurfaceInput, ReplaceGoalContinuationForSurfaceInput,
};
use crate::goal_tracker::{GoalTracker, GoalTurnResult, SAME_GAP_STREAK_LIMIT};

const ACTOR_MAILBOX_CAPACITY: usize = 32;
const GOAL_ACTOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(6);
static GOAL_RUNTIME_LEASES: OnceLock<Mutex<HashMap<PathBuf, Weak<GoalRuntimeLeaseInner>>>> =
    OnceLock::new();
#[cfg(test)]
static SURFACE_OUTER_TURN_FINISH_FAILURES: OnceLock<Mutex<HashMap<String, usize>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn inject_surface_outer_turn_finish_failure_once(session_id: &str) {
    SURFACE_OUTER_TURN_FINISH_FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id.to_string(), 1);
}

#[cfg(test)]
fn take_surface_outer_turn_finish_failure(session_id: &str) -> bool {
    let mut failures = SURFACE_OUTER_TURN_FINISH_FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(remaining) = failures.get_mut(session_id) else {
        return false;
    };
    *remaining -= 1;
    if *remaining == 0 {
        failures.remove(session_id);
    }
    true
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTurnContext {
    pub session_id: String,
    pub goal_id: GoalId,
    pub goal_run_id: orca_core::goal_runtime::GoalRunId,
    pub outer_turn_id: GoalOuterTurnId,
    pub origin: GoalTurnOrigin,
    pub run_started: bool,
}

#[derive(Clone, Debug)]
pub struct GoalRuntimeBinding {
    pub handle: GoalRuntimeHandle,
    pub turn: Option<GoalTurnContext>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalContinuationStatus {
    Ready,
    OuterTurnInFlight,
    PendingVerification,
    Inactive,
}

#[derive(Clone, Debug)]
pub struct GoalContinuationSnapshot {
    pub record: GoalRecord,
    pub status: GoalContinuationStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalActorError {
    Closed,
    Timeout { timeout: Duration },
    Store(String),
    Invalid(String),
    OwnerActive { path: String, message: String },
}

impl fmt::Display for GoalActorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("goal actor mailbox is closed"),
            Self::Timeout { timeout } => {
                write!(formatter, "goal actor reply timed out after {timeout:?}")
            }
            Self::Store(error) => write!(formatter, "goal actor store error: {error}"),
            Self::Invalid(error) => formatter.write_str(error),
            Self::OwnerActive { path, message } => {
                write!(
                    formatter,
                    "goal runtime is already owned for {path}: {message}"
                )
            }
        }
    }
}

impl std::error::Error for GoalActorError {}

impl From<GoalStoreError> for GoalActorError {
    fn from(error: GoalStoreError) -> Self {
        Self::Store(error.to_string())
    }
}

#[derive(Clone)]
pub struct GoalRuntimeHandle {
    sender: SyncSender<GoalActorCommand>,
    request_timeout: Duration,
}

impl fmt::Debug for GoalRuntimeHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GoalRuntimeHandle(..)")
    }
}

pub struct GoalActor {
    store: GoalStore,
    sender: Receiver<GoalActorCommand>,
    active: HashMap<String, ActiveGoalTurn>,
    trackers: HashMap<String, GoalTracker>,
    pending_verification: HashMap<String, PendingVerification>,
    pending_surface_decisions: HashMap<String, PendingSurfaceDecision>,
    pending_recoveries: HashMap<String, Vec<GoalRecoveryRecord>>,
    surface_owner_epoch: Option<u64>,
    _runtime_lease: Option<GoalRuntimeLease>,
}

struct GoalRuntimeLease {
    _inner: Arc<GoalRuntimeLeaseInner>,
}

struct GoalRuntimeLeaseInner {
    _lock: ExclusiveFileLock,
    owner_epoch: u64,
}

impl GoalRuntimeLease {
    fn acquire(store: &GoalStore) -> Result<(Self, bool), GoalActorError> {
        let database_path = store.path();
        let database_path =
            absolute_path(database_path).map_err(|error| GoalActorError::OwnerActive {
                path: database_path.display().to_string(),
                message: error.to_string(),
            })?;
        let registry = GOAL_RUNTIME_LEASES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(inner) = registry.get(&database_path).and_then(Weak::upgrade) {
            return Ok((Self { _inner: inner }, false));
        }

        let lock_path = database_path.with_extension("runtime.lock");
        let lock = ExclusiveFileLock::try_acquire(&lock_path).map_err(|error| {
            GoalActorError::OwnerActive {
                path: lock_path.display().to_string(),
                message: error.to_string(),
            }
        })?;
        let owner_epoch = store.claim_surface_owner_epoch()?;
        let inner = Arc::new(GoalRuntimeLeaseInner {
            _lock: lock,
            owner_epoch,
        });
        registry.insert(database_path, Arc::downgrade(&inner));
        Ok((Self { _inner: inner }, true))
    }

    fn owner_epoch(&self) -> u64 {
        self._inner.owner_epoch
    }
}

fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

struct ActiveGoalTurn {
    context: GoalTurnContext,
    tracker: GoalTracker,
    pending_pause: Option<PendingGoalPause>,
    surface_result: Option<RecordedSurfaceTurnResult>,
    surface_owned: bool,
    surface_identity: Option<Box<crate::runtime_surface::SurfaceGoalGenerationIdentity>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordedSurfaceTurnResult {
    result: GoalTurnResult,
    progress: GoalSurfaceTurnProgress,
}

struct PendingGoalPause {
    reason: orca_core::goal_runtime::GoalPauseReason,
    message: String,
}

struct PendingVerification {
    context: GoalTurnContext,
    tracker: GoalTracker,
}

struct PendingSurfaceDecision {
    identity: Box<crate::runtime_surface::SurfaceGoalGenerationIdentity>,
    status: GoalTurnStatus,
    usage: GoalUsage,
    verification: Option<GoalVerificationResult>,
    action: GoalNextAction,
    tracker: GoalTracker,
    progress: GoalSurfaceTurnProgress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GoalSurfaceDecisionPreview {
    pub(crate) action: GoalNextAction,
    pub(crate) progress: GoalSurfaceTurnProgress,
}

fn surface_evidence_matches(
    core: &orca_core::goal_runtime::EvidenceItem,
    surface: &crate::runtime_surface::SurfaceEvidenceItem,
) -> bool {
    let kind_matches = matches!(
        (core.kind, surface.kind),
        (
            orca_core::goal_runtime::EvidenceKind::Test,
            crate::runtime_surface::SurfaceEvidenceKind::Test
        ) | (
            orca_core::goal_runtime::EvidenceKind::File,
            crate::runtime_surface::SurfaceEvidenceKind::File
        ) | (
            orca_core::goal_runtime::EvidenceKind::Command,
            crate::runtime_surface::SurfaceEvidenceKind::Command
        ) | (
            orca_core::goal_runtime::EvidenceKind::Observation,
            crate::runtime_surface::SurfaceEvidenceKind::Observation
        ) | (
            orca_core::goal_runtime::EvidenceKind::External,
            crate::runtime_surface::SurfaceEvidenceKind::External
        )
    );
    kind_matches
        && core.summary == surface.summary.as_str()
        && core.target.as_deref() == surface.target.as_ref().map(|target| target.as_str())
}

fn surface_evidence_list_matches(
    core: &[orca_core::goal_runtime::EvidenceItem],
    surface: &[crate::runtime_surface::SurfaceEvidenceItem],
) -> bool {
    core.len() == surface.len()
        && core
            .iter()
            .zip(surface)
            .all(|(core, surface)| surface_evidence_matches(core, surface))
}

fn surface_blocker_matches(
    core: &orca_core::goal_runtime::BlockerSummary,
    surface: &crate::runtime_surface::SurfaceBlocker,
) -> bool {
    let kind_matches = matches!(
        (core.kind, surface.kind),
        (
            orca_core::goal_runtime::BlockerKind::UserDecision,
            crate::runtime_surface::SurfaceBlockerKind::UserDecision
        ) | (
            orca_core::goal_runtime::BlockerKind::MissingAuthority,
            crate::runtime_surface::SurfaceBlockerKind::MissingAuthority
        ) | (
            orca_core::goal_runtime::BlockerKind::ExternalState,
            crate::runtime_surface::SurfaceBlockerKind::ExternalState
        ) | (
            orca_core::goal_runtime::BlockerKind::EnvironmentContradiction,
            crate::runtime_surface::SurfaceBlockerKind::EnvironmentContradiction
        ) | (
            orca_core::goal_runtime::BlockerKind::UnverifiableRequirement,
            crate::runtime_surface::SurfaceBlockerKind::UnverifiableRequirement
        )
    );
    kind_matches
        && core.summary == surface.summary.as_str()
        && core.fingerprint == surface.fingerprint.as_str()
        && surface_evidence_list_matches(&core.evidence, &surface.evidence)
}

fn surface_verification_matches(
    core: &GoalVerificationResult,
    surface: &crate::runtime_surface::SurfaceGoalVerification,
) -> bool {
    match (core, surface) {
        (
            GoalVerificationResult::Achieved { evidence: core },
            crate::runtime_surface::SurfaceGoalVerification::Achieved { evidence: surface },
        ) => surface_evidence_list_matches(core, surface),
        (
            GoalVerificationResult::NotAchieved { gaps: core },
            crate::runtime_surface::SurfaceGoalVerification::NotAchieved { gaps: surface },
        ) => {
            core.len() == surface.len()
                && core.iter().zip(surface).all(|(core, surface)| {
                    core.summary == surface.summary.as_str()
                        && core.fingerprint == surface.fingerprint.as_str()
                        && core.model_fixable == surface.model_fixable
                })
        }
        (
            GoalVerificationResult::Blocked { blocker: core },
            crate::runtime_surface::SurfaceGoalVerification::Blocked { blocker: surface },
        ) => surface_blocker_matches(core, surface),
        (
            GoalVerificationResult::Indeterminate { message: core },
            crate::runtime_surface::SurfaceGoalVerification::Indeterminate { message: surface },
        ) => core == surface.as_str(),
        _ => false,
    }
}

fn surface_finish_matches_pending(
    input: &FinishGoalOuterTurnForSurfaceInput,
    pending: &PendingSurfaceDecision,
) -> bool {
    let status_matches = matches!(
        (pending.status, input.status),
        (
            GoalTurnStatus::Success,
            crate::runtime_surface::GoalOuterTurnStatus::Success
        ) | (
            GoalTurnStatus::Failed,
            crate::runtime_surface::GoalOuterTurnStatus::Failed
        ) | (
            GoalTurnStatus::Cancelled,
            crate::runtime_surface::GoalOuterTurnStatus::Cancelled
        ) | (
            GoalTurnStatus::ApprovalRequired,
            crate::runtime_surface::GoalOuterTurnStatus::ApprovalRequired
        ) | (
            GoalTurnStatus::BudgetExhausted,
            crate::runtime_surface::GoalOuterTurnStatus::BudgetExhausted
        )
    );
    let usage_matches = pending.usage.charged_input_tokens == input.usage.charged_input_tokens
        && pending.usage.output_tokens == input.usage.output_tokens
        && pending.usage.cache_tokens == input.usage.cache_tokens
        && pending.usage.verifier_tokens == input.usage.verifier_tokens
        && pending.usage.cost_micros == input.usage.cost_micros
        && pending.usage.elapsed_seconds == input.usage.elapsed_seconds;
    let action_matches = match (&pending.action, input.next_action) {
        (
            GoalNextAction::Continue { reason },
            crate::runtime_surface::GoalOuterTurnNextAction::Continue,
        ) => input.continuation.as_ref().is_some_and(|continuation| {
            matches!(
                (reason, continuation.reason),
                (
                    orca_core::goal_runtime::GoalContinuationReason::Progress,
                    crate::runtime_surface::GoalContinuationAdmitReason::Progress
                ) | (
                    orca_core::goal_runtime::GoalContinuationReason::GapFeedback,
                    crate::runtime_surface::GoalContinuationAdmitReason::GapFeedback
                )
            )
        }),
        (
            GoalNextAction::Verify { .. },
            crate::runtime_surface::GoalOuterTurnNextAction::Verify,
        ) => input.continuation.is_none(),
        (
            GoalNextAction::Pause { reason, message },
            crate::runtime_surface::GoalOuterTurnNextAction::Pause,
        ) => {
            let reason_matches = matches!(
                (reason, &input.stop_reason),
                (
                    orca_core::goal_runtime::GoalPauseReason::User,
                    crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Paused {
                            reason: crate::runtime_surface::SurfaceGoalPauseReason::User,
                            ..
                        },
                    }
                ) | (
                    orca_core::goal_runtime::GoalPauseReason::NoProgress,
                    crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Paused {
                            reason: crate::runtime_surface::SurfaceGoalPauseReason::NoProgress,
                            ..
                        },
                    }
                ) | (
                    orca_core::goal_runtime::GoalPauseReason::Backoff,
                    crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Paused {
                            reason: crate::runtime_surface::SurfaceGoalPauseReason::Backoff,
                            ..
                        },
                    }
                ) | (
                    orca_core::goal_runtime::GoalPauseReason::Infrastructure,
                    crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Paused {
                            reason: crate::runtime_surface::SurfaceGoalPauseReason::Infrastructure,
                            ..
                        },
                    }
                ) | (
                    orca_core::goal_runtime::GoalPauseReason::WaitingForWorkflow,
                    crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Paused {
                            reason:
                                crate::runtime_surface::SurfaceGoalPauseReason::WaitingForWorkflow,
                            ..
                        },
                    }
                ) | (
                    orca_core::goal_runtime::GoalPauseReason::Recovery,
                    crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Paused {
                            reason: crate::runtime_surface::SurfaceGoalPauseReason::Recovery,
                            ..
                        },
                    }
                ) | (
                    orca_core::goal_runtime::GoalPauseReason::UsageLimit,
                    crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Paused {
                            reason: crate::runtime_surface::SurfaceGoalPauseReason::UsageLimit,
                            ..
                        },
                    }
                )
            );
            let state_message_matches = matches!(
                &input.stop_reason,
                crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                    state: crate::runtime_surface::SurfaceGoalState::Paused {
                        message: surface,
                        ..
                    },
                } if surface.as_str() == message
            );
            input.continuation.is_none()
                && input.pause_message == *message
                && reason_matches
                && state_message_matches
        }
        (
            GoalNextAction::Blocked { blocker },
            crate::runtime_surface::GoalOuterTurnNextAction::Blocked,
        ) => {
            input.continuation.is_none()
                && matches!(
                    &input.stop_reason,
                    crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Blocked {
                            blocker: surface,
                        },
                    } if surface_blocker_matches(blocker, surface)
                )
        }
        (
            GoalNextAction::BudgetLimited,
            crate::runtime_surface::GoalOuterTurnNextAction::BudgetLimited,
        ) => {
            input.continuation.is_none()
                && matches!(
                    input.stop_reason,
                    crate::runtime_surface::GoalContinuationStopReason::BudgetLimited { .. }
                        | crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                            state: crate::runtime_surface::SurfaceGoalState::BudgetLimited,
                        }
                )
        }
        (
            GoalNextAction::Complete { evidence },
            crate::runtime_surface::GoalOuterTurnNextAction::Complete,
        ) => {
            input.continuation.is_none()
                && matches!(
                    &input.stop_reason,
                    crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Complete {
                            evidence: surface,
                        },
                    } if surface_evidence_list_matches(evidence, surface)
                )
        }
        _ => false,
    };
    let verification_matches = match (&pending.verification, &input.verification) {
        (None, None) => true,
        (Some(core), Some(surface)) => surface_verification_matches(core, surface),
        _ => false,
    };
    status_matches
        && usage_matches
        && input.progress == pending.progress
        && action_matches
        && verification_matches
}

enum GoalActorCommand {
    Read {
        session_id: String,
        reply: Reply,
    },
    Project {
        session_id: String,
        reply: Reply,
    },
    ContinuationState {
        session_id: String,
        reply: Reply,
    },
    RecentGapFingerprints {
        goal_id: GoalId,
        limit: u32,
        reply: Reply,
    },
    TakeRecoveries {
        session_id: String,
        reply: Reply,
    },
    Create {
        input: CreateGoalInput,
        reply: Reply,
    },
    CreateForSurface {
        input: CreateGoalInput,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    CreateAndPrepareRunForSurface {
        input: CreateGoalAndPrepareRunForSurfaceInput,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    AdoptForSurface {
        session_id: String,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    EditForSurface {
        session_id: String,
        expected_goal_id: GoalId,
        expected_goal_revision: u32,
        objective: String,
        token_budget_update: GoalSurfaceTokenBudgetUpdate,
        at: i64,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    ClearForSurface {
        session_id: String,
        expected_goal_id: GoalId,
        expected_goal_revision: u32,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    PrepareRunForSurface {
        input: PrepareGoalRunForSurfaceInput,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    EditAndPrepareRunForSurface {
        input: EditGoalAndPrepareRunForSurfaceInput,
        contexts: [GoalSurfaceMutationContext; 2],
        reply: Reply,
    },
    BeginOuterTurnForSurface {
        input: BeginGoalOuterTurnForSurfaceInput,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    RestoreOuterTurnForSurface {
        session_id: String,
        identity: Box<crate::runtime_surface::SurfaceGoalGenerationIdentity>,
        reply: Reply,
    },
    ReleaseOuterTurnForSurface {
        session_id: String,
        identity: Box<crate::runtime_surface::SurfaceGoalGenerationIdentity>,
        reply: Reply,
    },
    RecordTurnResultForSurface {
        session_id: String,
        result: RecordedSurfaceTurnResult,
        reply: Reply,
    },
    PauseForSurface {
        input: PauseGoalForSurfaceInput,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    PauseQuiescentForSurface {
        input: PauseQuiescentGoalForSurfaceInput,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    FinishOuterTurnForSurface {
        input: FinishGoalOuterTurnForSurfaceInput,
        contexts: Vec<GoalSurfaceMutationContext>,
        reply: Reply,
    },
    DecideOuterTurnForSurface {
        session_id: String,
        status: GoalTurnStatus,
        usage: GoalUsage,
        verification: Option<GoalVerificationResult>,
        reply: Reply,
    },
    RecoverRunForSurface {
        input: RecoverGoalRunForSurfaceInput,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    ReplaceContinuationWithRecoveryForSurface {
        input: ReplaceGoalContinuationForSurfaceInput,
        context: GoalSurfaceMutationContext,
        reply: Reply,
    },
    PendingSurfaceMutations {
        session_id: String,
        reply: Reply,
    },
    AcknowledgeSurfaceMutation {
        store_commit_id: String,
        receipt_digest: [u8; 32],
        reply: Reply,
    },
    RecordVerifierUsage {
        outer_turn_id: GoalOuterTurnId,
        event: GoalUsageEvent,
        reply: Reply,
    },
    Edit {
        session_id: String,
        objective: String,
        token_budget: Option<i64>,
        at: i64,
        reply: Reply,
    },
    LatestActive {
        reply: Reply,
    },
    ResumeInto {
        source_session_id: String,
        resumed_session_id: String,
        at: i64,
        reply: Reply,
    },
    Clear {
        session_id: String,
        reply: Reply,
    },
    BeginOuterTurn {
        session_id: String,
        origin: GoalTurnOrigin,
        provider_turn_id: String,
        started_at: i64,
        reply: Reply,
    },
    SubmitIntent {
        session_id: String,
        intent: GoalUpdateIntent,
        created_at: i64,
        reply: Reply,
    },
    FinishOuterTurn {
        session_id: String,
        status: GoalTurnStatus,
        end_reason: crate::lifecycle::TurnEndReason,
        terminal: Option<orca_core::budget::OperationTerminal>,
        usage: GoalUsage,
        tool_count: u32,
        model_response_count: u32,
        has_substantive_progress: bool,
        gap_fingerprint: Option<String>,
        finished_at: i64,
        reply: Reply,
    },
    Verify {
        session_id: String,
        result: GoalVerificationResult,
        at: i64,
        reply: Reply,
    },
    Pause {
        session_id: String,
        reason: orca_core::goal_runtime::GoalPauseReason,
        message: String,
        at: i64,
        reply: Reply,
    },
    Resume {
        session_id: String,
        origin: GoalTurnOrigin,
        at: i64,
        reply: Reply,
    },
    #[cfg(test)]
    DelayForTest {
        duration: Duration,
        started: SyncSender<()>,
        reply: Reply,
    },
    Shutdown,
}

impl GoalActorCommand {
    fn is_read_only(&self) -> bool {
        match self {
            Self::Read { .. }
            | Self::Project { .. }
            | Self::ContinuationState { .. }
            | Self::RecentGapFingerprints { .. }
            | Self::PendingSurfaceMutations { .. }
            | Self::LatestActive { .. } => true,
            #[cfg(test)]
            Self::DelayForTest { .. } => true,
            _ => false,
        }
    }
}

type Reply = SyncSender<Result<GoalActorReply, GoalActorError>>;

enum GoalActorReply {
    None,
    Record(Option<GoalRecord>),
    Projected(Option<ThreadGoal>),
    Continuation(Option<GoalContinuationSnapshot>),
    GapFingerprints(Vec<Option<String>>),
    Recoveries(Vec<GoalRecoveryRecord>),
    Created(GoalRecord),
    SurfaceMutation(GoalSurfaceMutationRecord),
    SurfaceMutations(Vec<GoalSurfaceMutationRecord>),
    Bool(bool),
    Usage(GoalUsage),
    Edited(Option<GoalRecord>),
    Latest(Option<ThreadGoal>),
    Turn(GoalTurnContext),
    Ack(GoalUpdateAck),
    Action(GoalNextAction),
    SurfaceDecisionPreview(GoalSurfaceDecisionPreview),
}

impl GoalRuntimeHandle {
    pub fn spawn(store: GoalStore) -> (Self, thread::JoinHandle<()>) {
        Self::spawn_with_owner(store, None, None, Vec::new())
    }

    fn spawn_with_lease(
        store: GoalStore,
        runtime_lease: Option<GoalRuntimeLease>,
        recoveries: Vec<GoalRecoveryRecord>,
    ) -> (Self, thread::JoinHandle<()>) {
        let surface_owner_epoch = runtime_lease.as_ref().map(GoalRuntimeLease::owner_epoch);
        Self::spawn_with_owner(store, runtime_lease, surface_owner_epoch, recoveries)
    }

    fn spawn_with_owner(
        store: GoalStore,
        runtime_lease: Option<GoalRuntimeLease>,
        surface_owner_epoch: Option<u64>,
        recoveries: Vec<GoalRecoveryRecord>,
    ) -> (Self, thread::JoinHandle<()>) {
        let (sender, receiver) = mpsc::sync_channel(ACTOR_MAILBOX_CAPACITY);
        let mut pending_recoveries = HashMap::<String, Vec<GoalRecoveryRecord>>::new();
        for recovery in recoveries {
            pending_recoveries
                .entry(recovery.session_id.clone())
                .or_default()
                .push(recovery);
        }
        let actor = GoalActor {
            store,
            sender: receiver,
            active: HashMap::new(),
            trackers: HashMap::new(),
            pending_verification: HashMap::new(),
            pending_surface_decisions: HashMap::new(),
            pending_recoveries,
            surface_owner_epoch,
            _runtime_lease: runtime_lease,
        };
        let join = thread::Builder::new()
            .name("orca-goal-actor".to_string())
            .spawn(move || actor.run())
            .expect("goal actor thread must start");
        (
            Self {
                sender,
                request_timeout: GOAL_ACTOR_REQUEST_TIMEOUT,
            },
            join,
        )
    }

    #[cfg(test)]
    fn spawn_with_request_timeout_for_test(
        store: GoalStore,
        request_timeout: Duration,
    ) -> (Self, thread::JoinHandle<()>) {
        let (mut handle, join) = Self::spawn(store);
        handle.request_timeout = request_timeout;
        (handle, join)
    }

    #[cfg(test)]
    fn spawn_surface_owned_for_test(store: GoalStore) -> (Self, thread::JoinHandle<()>) {
        let surface_owner_epoch = store
            .claim_surface_owner_epoch()
            .expect("test Goal surface owner epoch");
        Self::spawn_with_owner(store, None, Some(surface_owner_epoch), Vec::new())
    }

    pub fn open_default() -> Result<(Self, thread::JoinHandle<()>), GoalActorError> {
        let store = GoalStore::load_default()?;
        let (lease, first_owner_in_process) = GoalRuntimeLease::acquire(&store)?;
        let recoveries = if first_owner_in_process {
            store.recover_in_flight_runs()?
        } else {
            Vec::new()
        };
        Ok(Self::spawn_with_lease(store, Some(lease), recoveries))
    }

    #[cfg(test)]
    pub(crate) fn delay_for_test(
        &self,
        duration: Duration,
    ) -> Result<thread::JoinHandle<()>, GoalActorError> {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.sender
            .send(GoalActorCommand::DelayForTest {
                duration,
                started: started_tx,
                reply: reply_tx,
            })
            .map_err(|_| GoalActorError::Closed)?;
        started_rx.recv().map_err(|_| GoalActorError::Closed)?;
        Ok(thread::spawn(move || {
            let _ = reply_rx.recv();
        }))
    }

    pub fn read(&self, session_id: &str) -> Result<Option<GoalRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::Read {
            session_id: session_id.to_string(),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Record(record) => Ok(record),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong reply".to_string(),
            )),
        })
    }

    pub fn project_thread_goal(
        &self,
        session_id: &str,
    ) -> Result<Option<ThreadGoal>, GoalActorError> {
        self.request(|reply| GoalActorCommand::Project {
            session_id: session_id.to_string(),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Projected(goal) => Ok(goal),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong projection reply".to_string(),
            )),
        })
    }

    pub fn continuation_state(
        &self,
        session_id: &str,
    ) -> Result<Option<GoalContinuationSnapshot>, GoalActorError> {
        self.request(|reply| GoalActorCommand::ContinuationState {
            session_id: session_id.to_string(),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Continuation(state) => Ok(state),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong continuation reply".to_string(),
            )),
        })
    }

    /// Most recent gap fingerprint for the session's goal, if any.
    pub fn recent_gap_fingerprint(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, GoalActorError> {
        let Some(record) = self.read(session_id)? else {
            return Ok(None);
        };
        let history = self
            .request(|reply| GoalActorCommand::RecentGapFingerprints {
                goal_id: record.goal_id,
                limit: 1,
                reply,
            })
            .and_then(|reply| match reply {
                GoalActorReply::GapFingerprints(history) => Ok(history),
                _ => Err(GoalActorError::Invalid(
                    "goal actor returned wrong gap history reply".to_string(),
                )),
            })?;
        Ok(history.into_iter().next().flatten())
    }

    pub fn take_recoveries(
        &self,
        session_id: &str,
    ) -> Result<Vec<GoalRecoveryRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::TakeRecoveries {
            session_id: session_id.to_string(),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Recoveries(recoveries) => Ok(recoveries),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong recovery reply".to_string(),
            )),
        })
    }

    pub fn create(&self, input: CreateGoalInput) -> Result<GoalRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::Create { input, reply })
            .and_then(|reply| match reply {
                GoalActorReply::Created(goal) => Ok(goal),
                _ => Err(GoalActorError::Invalid(
                    "goal actor returned wrong create reply".to_string(),
                )),
            })
    }

    pub(crate) fn create_for_surface(
        &self,
        input: CreateGoalInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::CreateForSurface {
            input,
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface-create reply".to_string(),
            )),
        })
    }

    pub(crate) fn create_and_prepare_run_for_surface(
        &self,
        input: CreateGoalAndPrepareRunForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::CreateAndPrepareRunForSurface {
            input,
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface create-and-run reply".to_string(),
            )),
        })
    }

    pub(crate) fn adopt_for_surface(
        &self,
        session_id: &str,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::AdoptForSurface {
            session_id: session_id.to_string(),
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface-adoption reply".to_string(),
            )),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn edit_for_surface(
        &self,
        session_id: &str,
        expected_goal_id: GoalId,
        expected_goal_revision: u32,
        objective: impl Into<String>,
        token_budget_update: GoalSurfaceTokenBudgetUpdate,
        at: i64,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::EditForSurface {
            session_id: session_id.to_string(),
            expected_goal_id,
            expected_goal_revision,
            objective: objective.into(),
            token_budget_update,
            at,
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface-edit reply".to_string(),
            )),
        })
    }

    pub(crate) fn clear_for_surface(
        &self,
        session_id: &str,
        expected_goal_id: GoalId,
        expected_goal_revision: u32,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::ClearForSurface {
            session_id: session_id.to_string(),
            expected_goal_id,
            expected_goal_revision,
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface-clear reply".to_string(),
            )),
        })
    }

    pub(crate) fn prepare_run_for_surface(
        &self,
        input: PrepareGoalRunForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::PrepareRunForSurface {
            input,
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface run-preparation reply".to_string(),
            )),
        })
    }

    pub(crate) fn edit_and_prepare_run_for_surface(
        &self,
        input: EditGoalAndPrepareRunForSurfaceInput,
        contexts: [GoalSurfaceMutationContext; 2],
    ) -> Result<Vec<GoalSurfaceMutationRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::EditAndPrepareRunForSurface {
            input,
            contexts,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutations(mutations) => Ok(mutations),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface edit-and-run reply".to_string(),
            )),
        })
    }

    pub(crate) fn begin_outer_turn_for_surface(
        &self,
        input: BeginGoalOuterTurnForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::BeginOuterTurnForSurface {
            input,
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface outer-turn reply".to_string(),
            )),
        })
    }

    pub(crate) fn restore_outer_turn_for_surface(
        &self,
        session_id: &str,
        identity: crate::runtime_surface::SurfaceGoalGenerationIdentity,
    ) -> Result<GoalTurnContext, GoalActorError> {
        self.request(|reply| GoalActorCommand::RestoreOuterTurnForSurface {
            session_id: session_id.to_string(),
            identity: Box::new(identity),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Turn(context) => Ok(context),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface restoration reply".to_string(),
            )),
        })
    }

    pub(crate) fn release_outer_turn_for_surface(
        &self,
        session_id: &str,
        identity: crate::runtime_surface::SurfaceGoalGenerationIdentity,
    ) -> Result<bool, GoalActorError> {
        self.request(|reply| GoalActorCommand::ReleaseOuterTurnForSurface {
            session_id: session_id.to_string(),
            identity: Box::new(identity),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Bool(released) => Ok(released),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface release reply".to_string(),
            )),
        })
    }

    pub(crate) fn finish_outer_turn_for_surface(
        &self,
        input: FinishGoalOuterTurnForSurfaceInput,
        contexts: Vec<GoalSurfaceMutationContext>,
    ) -> Result<Vec<GoalSurfaceMutationRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::FinishOuterTurnForSurface {
            input,
            contexts,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutations(mutations) => Ok(mutations),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface outer-turn settlement reply".to_string(),
            )),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_turn_result_for_surface(
        &self,
        session_id: &str,
        status: GoalTurnStatus,
        end_reason: crate::lifecycle::TurnEndReason,
        terminal: Option<orca_core::budget::OperationTerminal>,
        usage: GoalUsage,
        tool_count: u32,
        model_response_count: u32,
        has_substantive_progress: bool,
        gap_fingerprint: Option<String>,
    ) -> Result<(), GoalActorError> {
        let progress = GoalSurfaceTurnProgress {
            tool_count,
            model_response_count,
            gap_fingerprint: gap_fingerprint.clone(),
        };
        let result = build_turn_result(
            status,
            end_reason,
            terminal,
            tool_count,
            model_response_count,
            has_substantive_progress,
            gap_fingerprint,
        );
        self.request(|reply| GoalActorCommand::RecordTurnResultForSurface {
            session_id: session_id.to_string(),
            result: RecordedSurfaceTurnResult {
                result: GoalTurnResult { usage, ..result },
                progress,
            },
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Bool(true) => Ok(()),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface turn-result reply".to_string(),
            )),
        })
    }

    pub(crate) fn preview_outer_turn_for_surface(
        &self,
        session_id: &str,
        status: GoalTurnStatus,
        usage: GoalUsage,
        verification: Option<GoalVerificationResult>,
    ) -> Result<GoalSurfaceDecisionPreview, GoalActorError> {
        self.request(|reply| GoalActorCommand::DecideOuterTurnForSurface {
            session_id: session_id.to_string(),
            status,
            usage,
            verification,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceDecisionPreview(preview) => Ok(preview),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface decision preview reply".to_string(),
            )),
        })
    }

    pub(crate) fn pause_for_surface(
        &self,
        input: PauseGoalForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::PauseForSurface {
            input,
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface pause reply".to_string(),
            )),
        })
    }

    pub(crate) fn pause_quiescent_for_surface(
        &self,
        input: PauseQuiescentGoalForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::PauseQuiescentForSurface {
            input,
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface quiescent pause reply".to_string(),
            )),
        })
    }

    pub(crate) fn recover_run_for_surface(
        &self,
        input: RecoverGoalRunForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::RecoverRunForSurface {
            input,
            context,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface recovery reply".to_string(),
            )),
        })
    }

    pub(crate) fn replace_continuation_with_recovery_for_surface(
        &self,
        input: ReplaceGoalContinuationForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        self.request(
            |reply| GoalActorCommand::ReplaceContinuationWithRecoveryForSurface {
                input,
                context,
                reply,
            },
        )
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutation(mutation) => Ok(mutation),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong continuation-recovery reply".to_string(),
            )),
        })
    }

    pub fn pending_surface_mutations(
        &self,
        session_id: &str,
    ) -> Result<Vec<GoalSurfaceMutationRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::PendingSurfaceMutations {
            session_id: session_id.to_string(),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::SurfaceMutations(mutations) => Ok(mutations),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong pending surface-mutation reply".to_string(),
            )),
        })
    }

    pub(crate) fn acknowledge_surface_mutation(
        &self,
        store_commit_id: &str,
        receipt_digest: &[u8; 32],
    ) -> Result<bool, GoalActorError> {
        self.request(|reply| GoalActorCommand::AcknowledgeSurfaceMutation {
            store_commit_id: store_commit_id.to_string(),
            receipt_digest: *receipt_digest,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Bool(acknowledged) => Ok(acknowledged),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong surface acknowledgement reply".to_string(),
            )),
        })
    }

    pub fn record_verifier_usage_once(
        &self,
        outer_turn_id: &GoalOuterTurnId,
        event: GoalUsageEvent,
    ) -> Result<GoalUsage, GoalActorError> {
        self.request(|reply| GoalActorCommand::RecordVerifierUsage {
            outer_turn_id: outer_turn_id.clone(),
            event,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Usage(usage) => Ok(usage),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong verifier usage reply".to_string(),
            )),
        })
    }

    pub fn clear(&self, session_id: &str) -> Result<(), GoalActorError> {
        self.request(|reply| GoalActorCommand::Clear {
            session_id: session_id.to_string(),
            reply,
        })
        .map(|_| ())
    }

    pub fn edit(
        &self,
        session_id: &str,
        objective: impl Into<String>,
        token_budget: Option<i64>,
        at: i64,
    ) -> Result<Option<GoalRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::Edit {
            session_id: session_id.to_string(),
            objective: objective.into(),
            token_budget,
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Edited(goal) => Ok(goal),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong edit reply".to_string(),
            )),
        })
    }

    pub fn latest_active(&self) -> Result<Option<ThreadGoal>, GoalActorError> {
        self.request(|reply| GoalActorCommand::LatestActive { reply })
            .and_then(|reply| match reply {
                GoalActorReply::Latest(goal) => Ok(goal),
                _ => Err(GoalActorError::Invalid(
                    "goal actor returned wrong latest-goal reply".to_string(),
                )),
            })
    }

    pub fn resume_into(
        &self,
        source_session_id: &str,
        resumed_session_id: &str,
        at: i64,
    ) -> Result<Option<GoalRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::ResumeInto {
            source_session_id: source_session_id.to_string(),
            resumed_session_id: resumed_session_id.to_string(),
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Record(goal) => Ok(goal),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong resume reply".to_string(),
            )),
        })
    }

    pub fn begin_outer_turn(
        &self,
        session_id: &str,
        origin: GoalTurnOrigin,
        provider_turn_id: impl Into<String>,
        started_at: i64,
    ) -> Result<GoalTurnContext, GoalActorError> {
        self.request(|reply| GoalActorCommand::BeginOuterTurn {
            session_id: session_id.to_string(),
            origin,
            provider_turn_id: provider_turn_id.into(),
            started_at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Turn(context) => Ok(context),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong turn reply".to_string(),
            )),
        })
    }

    pub fn submit_intent(
        &self,
        session_id: &str,
        intent: GoalUpdateIntent,
        created_at: i64,
    ) -> Result<GoalUpdateAck, GoalActorError> {
        self.request(|reply| GoalActorCommand::SubmitIntent {
            session_id: session_id.to_string(),
            intent,
            created_at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Ack(ack) => Ok(ack),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong intent reply".to_string(),
            )),
        })
    }

    pub fn finish_outer_turn(
        &self,
        session_id: &str,
        status: GoalTurnStatus,
        end_reason: crate::lifecycle::TurnEndReason,
        terminal: Option<orca_core::budget::OperationTerminal>,
        usage: GoalUsage,
        tool_count: u32,
        model_response_count: u32,
        gap_fingerprint: Option<String>,
        finished_at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        let has_substantive_progress =
            gap_fingerprint.is_none() && (tool_count != 0 || model_response_count != 0);
        self.finish_outer_turn_with_progress(
            session_id,
            status,
            end_reason,
            terminal,
            usage,
            tool_count,
            model_response_count,
            has_substantive_progress,
            gap_fingerprint,
            finished_at,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_outer_turn_with_progress(
        &self,
        session_id: &str,
        status: GoalTurnStatus,
        end_reason: crate::lifecycle::TurnEndReason,
        terminal: Option<orca_core::budget::OperationTerminal>,
        usage: GoalUsage,
        tool_count: u32,
        model_response_count: u32,
        has_substantive_progress: bool,
        gap_fingerprint: Option<String>,
        finished_at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        self.request(|reply| GoalActorCommand::FinishOuterTurn {
            session_id: session_id.to_string(),
            status,
            end_reason,
            terminal,
            usage,
            tool_count,
            model_response_count,
            has_substantive_progress,
            gap_fingerprint,
            finished_at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Action(action) => Ok(action),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong finish reply".to_string(),
            )),
        })
    }

    pub fn verify(
        &self,
        session_id: &str,
        result: GoalVerificationResult,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        self.request(|reply| GoalActorCommand::Verify {
            session_id: session_id.to_string(),
            result,
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Action(action) => Ok(action),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong verifier reply".to_string(),
            )),
        })
    }

    pub fn pause(
        &self,
        session_id: &str,
        reason: orca_core::goal_runtime::GoalPauseReason,
        message: impl Into<String>,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        self.request(|reply| GoalActorCommand::Pause {
            session_id: session_id.to_string(),
            reason,
            message: message.into(),
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Action(action) => Ok(action),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong pause reply".to_string(),
            )),
        })
    }

    pub fn resume(
        &self,
        session_id: &str,
        origin: GoalTurnOrigin,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        self.request(|reply| GoalActorCommand::Resume {
            session_id: session_id.to_string(),
            origin,
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Action(action) => Ok(action),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong resume reply".to_string(),
            )),
        })
    }

    pub fn shutdown(&self) -> Result<(), GoalActorError> {
        self.sender
            .send(GoalActorCommand::Shutdown)
            .map_err(|_| GoalActorError::Closed)
    }

    fn request(
        &self,
        command: impl FnOnce(Reply) -> GoalActorCommand,
    ) -> Result<GoalActorReply, GoalActorError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let deadline = std::time::Instant::now() + self.request_timeout;
        let command = command(reply_tx);
        let read_only = command.is_read_only();
        let mut pending = Some(command);
        loop {
            match self
                .sender
                .try_send(pending.take().expect("pending Goal command"))
            {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(command)) => {
                    pending = Some(command);
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(GoalActorError::Timeout {
                            timeout: self.request_timeout,
                        });
                    }
                    thread::sleep(remaining.min(Duration::from_millis(1)));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(GoalActorError::Closed);
                }
            }
        }

        if !read_only {
            return reply_rx.recv().map_err(|_| GoalActorError::Closed)?;
        }

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match reply_rx.recv_timeout(remaining) {
            Ok(reply) => reply,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(GoalActorError::Timeout {
                timeout: self.request_timeout,
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(GoalActorError::Closed),
        }
    }
}

impl GoalActor {
    fn authorize_surface_context(
        &self,
        mut context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationContext, GoalActorError> {
        let owner_epoch = self.surface_owner_epoch.ok_or_else(|| {
            GoalActorError::Invalid(
                "Goal surface mutation requires the durable runtime owner lease".to_string(),
            )
        })?;
        // Thread surface epochs and the process-wide Goal lease epoch are distinct
        // counters. Surface write entry points are crate-private; the actor lease is
        // the capability and stamps its own durable epoch at the storage boundary.
        context.goal_owner_epoch = owner_epoch;
        Ok(context)
    }

    fn run(mut self) {
        while let Ok(command) = self.sender.recv() {
            if matches!(command, GoalActorCommand::Shutdown) {
                break;
            }
            self.handle(command);
        }
    }

    fn handle(&mut self, command: GoalActorCommand) {
        let (reply, result) = match command {
            GoalActorCommand::Read { session_id, reply } => {
                (reply, self.read(&session_id).map(GoalActorReply::Record))
            }
            GoalActorCommand::Project { session_id, reply } => (
                reply,
                self.store
                    .project_thread_goal(&session_id)
                    .map(GoalActorReply::Projected)
                    .map_err(Into::into),
            ),
            GoalActorCommand::ContinuationState { session_id, reply } => (
                reply,
                self.continuation_state(&session_id)
                    .map(GoalActorReply::Continuation),
            ),
            GoalActorCommand::RecentGapFingerprints {
                goal_id,
                limit,
                reply,
            } => (
                reply,
                self.store
                    .recent_gap_fingerprints(&goal_id, limit)
                    .map(GoalActorReply::GapFingerprints)
                    .map_err(Into::into),
            ),
            GoalActorCommand::TakeRecoveries { session_id, reply } => (
                reply,
                Ok(GoalActorReply::Recoveries(
                    self.pending_recoveries
                        .remove(&session_id)
                        .unwrap_or_default(),
                )),
            ),
            GoalActorCommand::Create { input, reply } => (
                reply,
                self.store
                    .create_goal(input)
                    .map(GoalActorReply::Created)
                    .map_err(Into::into),
            ),
            GoalActorCommand::CreateForSurface {
                input,
                context,
                reply,
            } => (
                reply,
                self.authorize_surface_context(context)
                    .and_then(|context| {
                        self.store
                            .create_goal_for_surface(input, context)
                            .map_err(Into::into)
                    })
                    .map(GoalActorReply::SurfaceMutation),
            ),
            GoalActorCommand::CreateAndPrepareRunForSurface {
                input,
                context,
                reply,
            } => (
                reply,
                self.authorize_surface_context(context)
                    .and_then(|context| {
                        self.store
                            .create_goal_and_prepare_run_for_surface(input, context)
                            .map_err(Into::into)
                    })
                    .map(GoalActorReply::SurfaceMutation),
            ),
            GoalActorCommand::AdoptForSurface {
                session_id,
                context,
                reply,
            } => (
                reply,
                self.adopt_for_surface(&session_id, context)
                    .map(GoalActorReply::SurfaceMutation),
            ),
            GoalActorCommand::EditForSurface {
                session_id,
                expected_goal_id,
                expected_goal_revision,
                objective,
                token_budget_update,
                at,
                context,
                reply,
            } => (
                reply,
                self.edit_for_surface(
                    &session_id,
                    expected_goal_id,
                    expected_goal_revision,
                    &objective,
                    token_budget_update,
                    at,
                    context,
                )
                .map(GoalActorReply::SurfaceMutation),
            ),
            GoalActorCommand::ClearForSurface {
                session_id,
                expected_goal_id,
                expected_goal_revision,
                context,
                reply,
            } => (
                reply,
                self.clear_for_surface(
                    &session_id,
                    expected_goal_id,
                    expected_goal_revision,
                    context,
                )
                .map(GoalActorReply::SurfaceMutation),
            ),
            GoalActorCommand::PrepareRunForSurface {
                input,
                context,
                reply,
            } => (
                reply,
                self.authorize_surface_context(context)
                    .and_then(|context| {
                        self.store
                            .prepare_goal_run_for_surface(input, context)
                            .map_err(Into::into)
                    })
                    .map(GoalActorReply::SurfaceMutation),
            ),
            GoalActorCommand::EditAndPrepareRunForSurface {
                input,
                mut contexts,
                reply,
            } => {
                let result = (|| {
                    contexts[0] = self.authorize_surface_context(contexts[0].clone())?;
                    contexts[1] = self.authorize_surface_context(contexts[1].clone())?;
                    self.store
                        .edit_goal_and_prepare_run_for_surface(input, contexts)
                        .map_err(Into::into)
                })()
                .map(GoalActorReply::SurfaceMutations);
                (reply, result)
            }
            GoalActorCommand::BeginOuterTurnForSurface {
                input,
                context,
                reply,
            } => (
                reply,
                self.authorize_surface_context(context)
                    .and_then(|context| self.begin_outer_turn_for_surface(input, context))
                    .map(GoalActorReply::SurfaceMutation),
            ),
            GoalActorCommand::RestoreOuterTurnForSurface {
                session_id,
                identity,
                reply,
            } => (
                reply,
                self.restore_outer_turn_for_surface(&session_id, *identity)
                    .map(GoalActorReply::Turn),
            ),
            GoalActorCommand::ReleaseOuterTurnForSurface {
                session_id,
                identity,
                reply,
            } => (
                reply,
                Ok(GoalActorReply::Bool(
                    self.release_outer_turn_for_surface(&session_id, &identity),
                )),
            ),
            GoalActorCommand::FinishOuterTurnForSurface {
                input,
                mut contexts,
                reply,
            } => {
                let session_id = input.session_id.clone();
                let identity = input.identity.clone();
                let successor = input
                    .continuation
                    .as_ref()
                    .map(|continuation| continuation.successor.clone());
                let pending_decision = self.pending_surface_decisions.remove(&session_id);
                let result = (|| {
                    let requires_preview = input.continuation.is_some()
                        || matches!(
                            input.status,
                            crate::runtime_surface::GoalOuterTurnStatus::Success
                                | crate::runtime_surface::GoalOuterTurnStatus::BudgetExhausted
                        );
                    if requires_preview && pending_decision.is_none() {
                        return Err(GoalActorError::Invalid(
                            "surface Goal outer-turn finish lacks its previewed decision"
                                .to_string(),
                        ));
                    }
                    if pending_decision.as_ref().is_some_and(|decision| {
                        decision.identity.as_ref() != identity.as_ref()
                            || !surface_finish_matches_pending(&input, decision)
                    }) {
                        return Err(GoalActorError::Invalid(
                            "surface Goal outer-turn finish differs from its previewed decision"
                                .to_string(),
                        ));
                    }
                    for context in &mut contexts {
                        *context = self.authorize_surface_context(context.clone())?;
                    }
                    #[cfg(test)]
                    if take_surface_outer_turn_finish_failure(&session_id) {
                        return Err(GoalActorError::Store(
                            "injected surface Goal outer-turn finish failure".to_string(),
                        ));
                    }
                    self.store
                        .finish_goal_outer_turn_for_surface(input, contexts)
                        .map_err(Into::into)
                })();
                if result.is_ok() {
                    if let Some(successor) = successor {
                        let record = result
                            .as_ref()
                            .expect("checked Goal continuation result")
                            .last()
                            .and_then(|mutation| match &mutation.receipt.row_state {
                                GoalSurfaceRowState::Present(record) => Some(record),
                                GoalSurfaceRowState::Removed => None,
                            })
                            .expect("admitted Goal continuation retains its Goal");
                        let outer_turn_id = GoalOuterTurnId::parse(
                            successor.goal_outer_turn_id.as_str().to_string(),
                        )
                        .expect("validated Goal successor outer-turn id");
                        let goal_run_id = orca_core::goal_runtime::GoalRunId::parse(
                            successor.goal_run_id.as_str().to_string(),
                        )
                        .expect("validated Goal successor run id");
                        let mut tracker = pending_decision
                            .as_ref()
                            .expect("successful Goal finish has its previewed decision")
                            .tracker
                            .clone();
                        tracker
                            .bind_persisted_outer_turn(
                                outer_turn_id.clone(),
                                GoalTurnOrigin::Continuation,
                            )
                            .expect("durable Goal successor binds to its tracker");
                        self.active.insert(
                            session_id.clone(),
                            ActiveGoalTurn {
                                context: GoalTurnContext {
                                    session_id: session_id.clone(),
                                    goal_id: record.goal_id.clone(),
                                    goal_run_id,
                                    outer_turn_id,
                                    origin: GoalTurnOrigin::Continuation,
                                    run_started: false,
                                },
                                tracker: tracker.clone(),
                                pending_pause: None,
                                surface_result: None,
                                surface_owned: true,
                                surface_identity: Some(successor),
                            },
                        );
                        self.trackers.insert(session_id.clone(), tracker);
                    } else {
                        self.active.remove(&session_id);
                        self.trackers.remove(&session_id);
                    }
                    self.pending_verification.remove(&session_id);
                } else if let Some(pending_decision) = pending_decision {
                    self.pending_surface_decisions
                        .insert(session_id.clone(), pending_decision);
                }
                let result = result.map(GoalActorReply::SurfaceMutations);
                (reply, result)
            }
            GoalActorCommand::DecideOuterTurnForSurface {
                session_id,
                status,
                usage,
                verification,
                reply,
            } => (
                reply,
                self.preview_outer_turn_for_surface(&session_id, status, usage, verification)
                    .map(GoalActorReply::SurfaceDecisionPreview),
            ),
            GoalActorCommand::RecordTurnResultForSurface {
                session_id,
                result,
                reply,
            } => {
                let recorded = self
                    .active
                    .get_mut(&session_id)
                    .ok_or_else(|| GoalActorError::Invalid("no active Goal outer turn".to_string()))
                    .and_then(|active| {
                        if !active.surface_owned {
                            return Err(GoalActorError::Invalid(
                                "Goal outer turn is not owned by the typed surface".to_string(),
                            ));
                        }
                        match active.surface_result.as_ref() {
                            Some(existing) if existing != &result => Err(GoalActorError::Invalid(
                                "surface Goal turn result conflicts with the recorded result"
                                    .to_string(),
                            )),
                            Some(_) => Ok(true),
                            None => {
                                active.surface_result = Some(result);
                                Ok(true)
                            }
                        }
                    })
                    .map(GoalActorReply::Bool);
                (reply, recorded)
            }
            GoalActorCommand::PauseForSurface {
                input,
                context,
                reply,
            } => (
                reply,
                self.authorize_surface_context(context)
                    .and_then(|context| {
                        self.store
                            .pause_goal_for_surface(input, context)
                            .map_err(Into::into)
                    })
                    .map(GoalActorReply::SurfaceMutation),
            ),
            GoalActorCommand::PauseQuiescentForSurface {
                input,
                context,
                reply,
            } => (
                reply,
                self.authorize_surface_context(context)
                    .and_then(|context| {
                        self.store
                            .pause_quiescent_goal_for_surface(input, context)
                            .map_err(Into::into)
                    })
                    .map(GoalActorReply::SurfaceMutation),
            ),
            GoalActorCommand::RecoverRunForSurface {
                input,
                context,
                reply,
            } => {
                let session_id = input.session_id.clone();
                let result = self.authorize_surface_context(context).and_then(|context| {
                    self.store
                        .recover_goal_run_for_surface(input, context)
                        .map_err(Into::into)
                });
                if result.is_ok() {
                    self.active.remove(&session_id);
                    self.trackers.remove(&session_id);
                    self.pending_verification.remove(&session_id);
                    self.pending_surface_decisions.remove(&session_id);
                }
                (reply, result.map(GoalActorReply::SurfaceMutation))
            }
            GoalActorCommand::ReplaceContinuationWithRecoveryForSurface {
                input,
                context,
                reply,
            } => {
                let session_id = input.interrupted.session_id.clone();
                let result = self.authorize_surface_context(context).and_then(|context| {
                    self.store
                        .replace_goal_continuation_with_recovery_for_surface(input, context)
                        .map_err(Into::into)
                });
                if result.is_ok() {
                    self.active.remove(&session_id);
                    self.trackers.remove(&session_id);
                    self.pending_verification.remove(&session_id);
                    self.pending_surface_decisions.remove(&session_id);
                }
                (reply, result.map(GoalActorReply::SurfaceMutation))
            }
            GoalActorCommand::PendingSurfaceMutations { session_id, reply } => (
                reply,
                self.store
                    .pending_surface_mutations(&session_id)
                    .map(GoalActorReply::SurfaceMutations)
                    .map_err(Into::into),
            ),
            GoalActorCommand::AcknowledgeSurfaceMutation {
                store_commit_id,
                receipt_digest,
                reply,
            } => {
                let result = self
                    .surface_owner_epoch
                    .ok_or_else(|| {
                        GoalActorError::Invalid(
                            "Goal surface acknowledgement requires the durable runtime owner lease"
                                .to_string(),
                        )
                    })
                    .and_then(|owner_epoch| {
                        self.store
                            .acknowledge_surface_mutation(
                                &store_commit_id,
                                &receipt_digest,
                                owner_epoch,
                            )
                            .map(GoalActorReply::Bool)
                            .map_err(Into::into)
                    });
                (reply, result)
            }
            GoalActorCommand::RecordVerifierUsage {
                outer_turn_id,
                event,
                reply,
            } => (
                reply,
                self.store
                    .record_verifier_usage_once(&outer_turn_id, event)
                    .map(GoalActorReply::Usage)
                    .map_err(Into::into),
            ),
            GoalActorCommand::Edit {
                session_id,
                objective,
                token_budget,
                at,
                reply,
            } => (
                reply,
                self.edit(&session_id, &objective, token_budget, at)
                    .map(GoalActorReply::Edited),
            ),
            GoalActorCommand::LatestActive { reply } => (
                reply,
                self.store
                    .latest_active()
                    .map(GoalActorReply::Latest)
                    .map_err(Into::into),
            ),
            GoalActorCommand::ResumeInto {
                source_session_id,
                resumed_session_id,
                at,
                reply,
            } => (
                reply,
                self.resume_into(&source_session_id, &resumed_session_id, at)
                    .map(GoalActorReply::Record),
            ),
            GoalActorCommand::Clear { session_id, reply } => {
                (reply, self.clear(&session_id).map(|_| GoalActorReply::None))
            }
            GoalActorCommand::BeginOuterTurn {
                session_id,
                origin,
                provider_turn_id,
                started_at,
                reply,
            } => (
                reply,
                self.begin_outer_turn(&session_id, origin, provider_turn_id, started_at)
                    .map(GoalActorReply::Turn),
            ),
            GoalActorCommand::SubmitIntent {
                session_id,
                intent,
                created_at,
                reply,
            } => (
                reply,
                self.submit_intent(&session_id, intent, created_at)
                    .map(GoalActorReply::Ack),
            ),
            GoalActorCommand::FinishOuterTurn {
                session_id,
                status,
                end_reason,
                terminal,
                usage,
                tool_count,
                model_response_count,
                has_substantive_progress,
                gap_fingerprint,
                finished_at,
                reply,
            } => (
                reply,
                self.finish_outer_turn(
                    &session_id,
                    status,
                    end_reason,
                    terminal,
                    usage,
                    tool_count,
                    model_response_count,
                    has_substantive_progress,
                    gap_fingerprint,
                    finished_at,
                )
                .map(GoalActorReply::Action),
            ),
            GoalActorCommand::Verify {
                session_id,
                result,
                at,
                reply,
            } => (
                reply,
                self.verify(&session_id, result, at)
                    .map(GoalActorReply::Action),
            ),
            GoalActorCommand::Pause {
                session_id,
                reason,
                message,
                at,
                reply,
            } => (
                reply,
                self.pause(&session_id, reason, message, at)
                    .map(GoalActorReply::Action),
            ),
            GoalActorCommand::Resume {
                session_id,
                origin,
                at,
                reply,
            } => (
                reply,
                self.resume(&session_id, origin, at)
                    .map(GoalActorReply::Action),
            ),
            #[cfg(test)]
            GoalActorCommand::DelayForTest {
                duration,
                started,
                reply,
            } => {
                let _ = started.send(());
                thread::sleep(duration);
                (reply, Ok(GoalActorReply::None))
            }
            GoalActorCommand::Shutdown => unreachable!(),
        };
        let _ = reply.send(result);
    }

    fn read(&self, session_id: &str) -> Result<Option<GoalRecord>, GoalActorError> {
        self.store.get_by_session(session_id).map_err(Into::into)
    }

    fn clear(&mut self, session_id: &str) -> Result<(), GoalActorError> {
        self.ensure_no_active_turn(session_id, "clear")?;
        self.store.clear_goal(session_id)?;
        self.active.remove(session_id);
        self.trackers.remove(session_id);
        self.pending_verification.remove(session_id);
        Ok(())
    }

    fn clear_for_surface(
        &mut self,
        session_id: &str,
        expected_goal_id: GoalId,
        expected_goal_revision: u32,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        let context = self.authorize_surface_context(context)?;
        self.ensure_no_active_turn(session_id, "clear")?;
        let mutation = self.store.clear_goal_for_surface(
            session_id,
            &expected_goal_id,
            expected_goal_revision,
            context,
        )?;
        self.active.remove(session_id);
        self.trackers.remove(session_id);
        self.pending_verification.remove(session_id);
        Ok(mutation)
    }

    fn adopt_for_surface(
        &mut self,
        session_id: &str,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        let context = self.authorize_surface_context(context)?;
        self.ensure_no_active_turn(session_id, "adopt")?;
        self.store
            .adopt_goal_for_surface(session_id, context)
            .map_err(Into::into)
    }

    fn edit(
        &mut self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
        at: i64,
    ) -> Result<Option<GoalRecord>, GoalActorError> {
        self.ensure_no_active_turn(session_id, "edit")?;
        let record = self
            .store
            .edit_goal(session_id, objective, token_budget, at)?;
        self.active.remove(session_id);
        self.trackers.remove(session_id);
        self.pending_verification.remove(session_id);
        Ok(record)
    }

    #[allow(clippy::too_many_arguments)]
    fn edit_for_surface(
        &mut self,
        session_id: &str,
        expected_goal_id: GoalId,
        expected_goal_revision: u32,
        objective: &str,
        token_budget_update: GoalSurfaceTokenBudgetUpdate,
        at: i64,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        let context = self.authorize_surface_context(context)?;
        self.ensure_no_active_turn(session_id, "edit")?;
        let mutation = self.store.edit_goal_for_surface(
            session_id,
            &expected_goal_id,
            expected_goal_revision,
            objective,
            token_budget_update,
            at,
            context,
        )?;
        self.active.remove(session_id);
        self.trackers.remove(session_id);
        self.pending_verification.remove(session_id);
        Ok(mutation)
    }

    fn resume_into(
        &mut self,
        source_session_id: &str,
        resumed_session_id: &str,
        at: i64,
    ) -> Result<Option<GoalRecord>, GoalActorError> {
        self.ensure_no_active_turn(source_session_id, "resume")?;
        self.ensure_no_active_turn(resumed_session_id, "resume")?;
        let record = self
            .store
            .resume_into(source_session_id, resumed_session_id, at)?;
        self.active.remove(source_session_id);
        self.trackers.remove(source_session_id);
        self.pending_verification.remove(source_session_id);
        self.active.remove(resumed_session_id);
        self.trackers.remove(resumed_session_id);
        self.pending_verification.remove(resumed_session_id);
        Ok(record)
    }

    fn continuation_state(
        &self,
        session_id: &str,
    ) -> Result<Option<GoalContinuationSnapshot>, GoalActorError> {
        let Some(record) = self.store.get_by_session(session_id)? else {
            return Ok(None);
        };
        let status = if self.pending_verification.contains_key(session_id) {
            GoalContinuationStatus::PendingVerification
        } else if self.active.contains_key(session_id) {
            GoalContinuationStatus::OuterTurnInFlight
        } else if record.state.should_continue() {
            GoalContinuationStatus::Ready
        } else {
            GoalContinuationStatus::Inactive
        };
        Ok(Some(GoalContinuationSnapshot { record, status }))
    }

    fn ensure_no_active_turn(&self, session_id: &str, action: &str) -> Result<(), GoalActorError> {
        if self.active.contains_key(session_id) {
            return Err(GoalActorError::Invalid(format!(
                "cannot {action} goal while an outer turn is in flight"
            )));
        }
        Ok(())
    }

    fn begin_outer_turn_for_surface(
        &mut self,
        input: BeginGoalOuterTurnForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalActorError> {
        if self.active.contains_key(&input.session_id) {
            return Err(GoalActorError::Invalid(
                "goal already has an active outer turn".to_string(),
            ));
        }
        if self.pending_verification.contains_key(&input.session_id) {
            return Err(GoalActorError::Invalid(
                "goal has a terminal intent pending verification".to_string(),
            ));
        }
        let identity = input.identity.clone();
        self.pending_surface_decisions.remove(&input.session_id);
        let mutation = self
            .store
            .begin_goal_outer_turn_for_surface(input, context)?;
        let GoalSurfaceRowState::Present(record) = &mutation.receipt.row_state else {
            return Err(GoalActorError::Invalid(
                "surface outer-turn receipt removed its Goal".to_string(),
            ));
        };
        let origin = match identity.outer_turn_origin {
            crate::runtime_surface::GoalOuterTurnOrigin::User => GoalTurnOrigin::User,
            crate::runtime_surface::GoalOuterTurnOrigin::Resume => GoalTurnOrigin::Resume,
            crate::runtime_surface::GoalOuterTurnOrigin::Continuation => {
                GoalTurnOrigin::Continuation
            }
            crate::runtime_surface::GoalOuterTurnOrigin::WorkflowNotification => {
                GoalTurnOrigin::WorkflowNotification
            }
        };
        let outer_turn_id =
            GoalOuterTurnId::parse(identity.goal_outer_turn_id.as_str().to_string())
                .map_err(GoalActorError::Invalid)?;
        let goal_run_id =
            orca_core::goal_runtime::GoalRunId::parse(identity.goal_run_id.as_str().to_string())
                .map_err(GoalActorError::Invalid)?;
        if record
            .current_run
            .as_ref()
            .is_none_or(|run| run.goal_run_id != goal_run_id || !run.in_flight)
        {
            return Err(GoalActorError::Invalid(
                "surface outer-turn receipt lost its in-flight run".to_string(),
            ));
        }
        let mut tracker = GoalTracker::from_record(record);
        tracker
            .bind_persisted_outer_turn(outer_turn_id.clone(), origin)
            .map_err(|error| GoalActorError::Invalid(error.to_string()))?;
        let turn = GoalTurnContext {
            session_id: mutation.session_id.clone(),
            goal_id: record.goal_id.clone(),
            goal_run_id,
            outer_turn_id,
            origin,
            run_started: false,
        };
        self.active.insert(
            mutation.session_id.clone(),
            ActiveGoalTurn {
                context: turn,
                tracker,
                pending_pause: None,
                surface_result: None,
                surface_owned: true,
                surface_identity: Some(identity),
            },
        );
        Ok(mutation)
    }

    fn restore_outer_turn_for_surface(
        &mut self,
        session_id: &str,
        identity: crate::runtime_surface::SurfaceGoalGenerationIdentity,
    ) -> Result<GoalTurnContext, GoalActorError> {
        self.pending_surface_decisions.remove(session_id);
        let record = self
            .store
            .validate_surface_outer_turn_binding(session_id, &identity)?;
        let origin = match identity.outer_turn_origin {
            crate::runtime_surface::GoalOuterTurnOrigin::User => GoalTurnOrigin::User,
            crate::runtime_surface::GoalOuterTurnOrigin::Resume => GoalTurnOrigin::Resume,
            crate::runtime_surface::GoalOuterTurnOrigin::Continuation => {
                GoalTurnOrigin::Continuation
            }
            crate::runtime_surface::GoalOuterTurnOrigin::WorkflowNotification => {
                GoalTurnOrigin::WorkflowNotification
            }
        };
        let outer_turn_id =
            GoalOuterTurnId::parse(identity.goal_outer_turn_id.as_str().to_string())
                .map_err(GoalActorError::Invalid)?;
        let goal_run_id =
            orca_core::goal_runtime::GoalRunId::parse(identity.goal_run_id.as_str().to_string())
                .map_err(GoalActorError::Invalid)?;
        if let Some(active) = self.active.get_mut(session_id) {
            if !active.surface_owned
                || active.context.goal_id != record.goal_id
                || active.context.goal_run_id != goal_run_id
                || active.context.outer_turn_id != outer_turn_id
                || active.context.origin != origin
            {
                return Err(GoalActorError::Invalid(
                    "active Goal outer turn differs from its recovery binding".to_string(),
                ));
            }
            active.surface_identity = Some(Box::new(identity));
            return Ok(active.context.clone());
        }
        let history = self
            .store
            .recent_gap_fingerprints(&record.goal_id, SAME_GAP_STREAK_LIMIT)?;
        let mut tracker = GoalTracker::from_record_with_history(&record, &history);
        tracker
            .bind_persisted_outer_turn(outer_turn_id.clone(), origin)
            .map_err(|error| GoalActorError::Invalid(error.to_string()))?;
        let turn = GoalTurnContext {
            session_id: session_id.to_string(),
            goal_id: record.goal_id,
            goal_run_id,
            outer_turn_id,
            origin,
            run_started: false,
        };
        self.active.insert(
            session_id.to_string(),
            ActiveGoalTurn {
                context: turn.clone(),
                tracker: tracker.clone(),
                pending_pause: None,
                surface_result: None,
                surface_owned: true,
                surface_identity: Some(Box::new(identity)),
            },
        );
        self.trackers.insert(session_id.to_string(), tracker);
        Ok(turn)
    }

    fn release_outer_turn_for_surface(
        &mut self,
        session_id: &str,
        identity: &crate::runtime_surface::SurfaceGoalGenerationIdentity,
    ) -> bool {
        let exact = self.active.get(session_id).is_some_and(|active| {
            active.surface_owned
                && active.surface_identity.as_deref() == Some(identity)
                && active.context.outer_turn_id.as_str() == identity.goal_outer_turn_id.as_str()
        });
        if exact {
            self.active.remove(session_id);
            self.trackers.remove(session_id);
            self.pending_surface_decisions.remove(session_id);
        }
        exact
    }

    fn preview_outer_turn_for_surface(
        &mut self,
        session_id: &str,
        status: GoalTurnStatus,
        usage: GoalUsage,
        verification: Option<GoalVerificationResult>,
    ) -> Result<GoalSurfaceDecisionPreview, GoalActorError> {
        let preview_usage = usage.clone();
        let preview_verification = verification.clone();
        let (identity, pending_pause, mut tracker, recorded_result) = {
            let active = self
                .active
                .get(session_id)
                .ok_or_else(|| GoalActorError::Invalid("no active Goal outer turn".to_string()))?;
            if !active.surface_owned {
                return Err(GoalActorError::Invalid(
                    "Goal outer turn is not owned by the typed surface".to_string(),
                ));
            }
            let identity = active.surface_identity.clone().ok_or_else(|| {
                GoalActorError::Invalid(
                    "surface Goal outer turn lacks its generation identity".to_string(),
                )
            })?;
            (
                identity,
                active
                    .pending_pause
                    .as_ref()
                    .map(|pause| (pause.reason, pause.message.clone())),
                active.tracker.clone(),
                active.surface_result.clone(),
            )
        };
        let fallback_progress = GoalSurfaceTurnProgress {
            tool_count: 0,
            model_response_count: 0,
            gap_fingerprint: Some(
                crate::goal_tracker::NO_SUBSTANTIVE_PROGRESS_GAP_FINGERPRINT.to_string(),
            ),
        };
        let (turn_result, progress) = match recorded_result {
            Some(recorded)
                if recorded.result.status == status && recorded.result.usage == usage =>
            {
                (recorded.result, recorded.progress)
            }
            Some(_) => {
                return Err(GoalActorError::Invalid(
                    "surface Goal preview disagrees with its recorded turn result".to_string(),
                ));
            }
            None => (
                GoalTurnResult {
                    usage,
                    ..build_turn_result(
                        status,
                        crate::lifecycle::TurnEndReason::Unclassified,
                        None,
                        0,
                        0,
                        false,
                        fallback_progress.gap_fingerprint.clone(),
                    )
                },
                fallback_progress,
            ),
        };
        let tracker_action = tracker
            .finish_outer_turn(turn_result)
            .map_err(|error| GoalActorError::Invalid(error.to_string()))?;
        let action = if let Some((reason, message)) = pending_pause {
            tracker.pause(reason, message)
        } else {
            tracker_action
        };
        let action = match (action, verification) {
            (GoalNextAction::Verify { .. }, Some(result)) => {
                tracker.apply_verification(result).clone()
            }
            (action, None) => action,
            (_, Some(_)) => {
                return Err(GoalActorError::Invalid(
                    "surface verification does not match a pending terminal intent".to_string(),
                ));
            }
        };
        self.pending_surface_decisions.insert(
            session_id.to_string(),
            PendingSurfaceDecision {
                identity,
                status,
                usage: preview_usage,
                verification: preview_verification,
                action: action.clone(),
                tracker,
                progress: progress.clone(),
            },
        );
        Ok(GoalSurfaceDecisionPreview { action, progress })
    }

    fn begin_outer_turn(
        &mut self,
        session_id: &str,
        origin: GoalTurnOrigin,
        provider_turn_id: String,
        started_at: i64,
    ) -> Result<GoalTurnContext, GoalActorError> {
        if self.active.contains_key(session_id) {
            return Err(GoalActorError::Invalid(
                "goal already has an active outer turn".to_string(),
            ));
        }
        if self.pending_verification.contains_key(session_id) {
            return Err(GoalActorError::Invalid(
                "goal has a terminal intent pending verification".to_string(),
            ));
        }
        let record = self
            .store
            .get_by_session(session_id)?
            .ok_or_else(|| GoalActorError::Invalid("goal does not exist".to_string()))?;
        if !record.state.should_continue() {
            return Err(GoalActorError::Invalid(format!(
                "goal is not active: {:?}",
                record.state
            )));
        }
        let run_id = record
            .current_run
            .as_ref()
            .filter(|run| !run.in_flight)
            .map(|run| run.goal_run_id.clone())
            .unwrap_or_default();
        let run_started = record.current_run.is_none();
        let run_id = if run_started {
            let run_id = run_id;
            self.store.begin_run(BeginGoalRunInput {
                goal_id: record.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin,
                started_at,
            })?;
            run_id
        } else {
            run_id
        };
        let mut tracker = match self.trackers.remove(session_id) {
            Some(tracker) => tracker,
            None => {
                let history = self
                    .store
                    .recent_gap_fingerprints(&record.goal_id, SAME_GAP_STREAK_LIMIT)?;
                GoalTracker::from_record_with_history(&record, &history)
            }
        };
        let outer_turn_id = tracker
            .begin_outer_turn(origin)
            .map_err(|error| GoalActorError::Invalid(error.to_string()))?;
        self.store.begin_outer_turn(BeginOuterTurnInput {
            goal_id: record.goal_id.clone(),
            goal_run_id: run_id.clone(),
            outer_turn_id: outer_turn_id.clone(),
            origin,
            provider_turn_id,
            started_at,
        })?;
        let context = GoalTurnContext {
            session_id: session_id.to_string(),
            goal_id: record.goal_id,
            goal_run_id: run_id,
            outer_turn_id,
            origin,
            run_started,
        };
        self.active.insert(
            session_id.to_string(),
            ActiveGoalTurn {
                context: context.clone(),
                tracker,
                pending_pause: None,
                surface_result: None,
                surface_owned: false,
                surface_identity: None,
            },
        );
        Ok(context)
    }

    fn submit_intent(
        &mut self,
        session_id: &str,
        intent: GoalUpdateIntent,
        created_at: i64,
    ) -> Result<GoalUpdateAck, GoalActorError> {
        let active = self
            .active
            .get_mut(session_id)
            .ok_or_else(|| GoalActorError::Invalid("no active goal outer turn".to_string()))?;
        let ack = active.tracker.submit_terminal_intent(intent.clone());
        if matches!(ack, GoalUpdateAck::DeferredToTurnEnd { .. }) {
            let record = GoalIntentRecord {
                outer_turn_id: active.context.outer_turn_id.clone(),
                intent,
                ack: ack.clone(),
                created_at,
            };
            if active.surface_owned {
                let owner_epoch = self.surface_owner_epoch.ok_or_else(|| {
                    GoalActorError::Invalid(
                        "surface Goal intent lacks its runtime owner epoch".to_string(),
                    )
                })?;
                let digest_input = serde_json::to_vec(&(
                    &record.outer_turn_id,
                    &record.intent,
                    &record.ack,
                    record.created_at,
                ))
                .map_err(|error| GoalActorError::Invalid(error.to_string()))?;
                let context = |kind: &[u8]| {
                    let mut hasher = Sha256::new();
                    hasher.update(kind);
                    hasher.update(&digest_input);
                    GoalSurfaceMutationContext {
                        store_commit_id: uuid::Uuid::now_v7().to_string(),
                        command_digest: hasher.finalize().into(),
                        goal_owner_epoch: owner_epoch,
                    }
                };
                let identity = active.surface_identity.clone().ok_or_else(|| {
                    GoalActorError::Invalid(
                        "surface Goal intent lacks its generation identity".to_string(),
                    )
                })?;
                let (persisted_ack, _) = self.store.record_intent_for_surface(
                    record,
                    identity,
                    [
                        context(b"intent_requested"),
                        context(b"intent_acknowledged"),
                    ],
                )?;
                return Ok(persisted_ack);
            } else {
                self.store.record_intent(record)?;
            }
        }
        Ok(ack)
    }

    fn finish_outer_turn(
        &mut self,
        session_id: &str,
        status: GoalTurnStatus,
        end_reason: crate::lifecycle::TurnEndReason,
        terminal: Option<orca_core::budget::OperationTerminal>,
        usage: GoalUsage,
        tool_count: u32,
        model_response_count: u32,
        has_substantive_progress: bool,
        gap_fingerprint: Option<String>,
        finished_at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        let effective_gap_fingerprint = gap_fingerprint.or_else(|| {
            (!has_substantive_progress)
                .then(|| crate::goal_tracker::NO_SUBSTANTIVE_PROGRESS_GAP_FINGERPRINT.to_string())
        });
        let mut turn_result = build_turn_result(
            status,
            end_reason,
            terminal,
            tool_count,
            model_response_count,
            has_substantive_progress,
            effective_gap_fingerprint.clone(),
        );
        turn_result.usage = usage.clone();
        let mut active = self
            .active
            .remove(session_id)
            .ok_or_else(|| GoalActorError::Invalid("no active goal outer turn".to_string()))?;
        let requested_pause = active.pending_pause.take();
        let tracker_action = active
            .tracker
            .finish_outer_turn(turn_result)
            .map_err(|error| GoalActorError::Invalid(error.to_string()))?;
        let action = if let Some(pause) = requested_pause.as_ref() {
            active.tracker.pause(pause.reason, pause.message.clone())
        } else {
            tracker_action
        };
        self.store.finish_outer_turn(FinishOuterTurnInput {
            goal_id: active.context.goal_id.clone(),
            goal_run_id: active.context.goal_run_id.clone(),
            outer_turn_id: active.context.outer_turn_id.clone(),
            status,
            tool_count,
            model_response_count,
            // NULL is a durable progress barrier. A fingerprint is persisted
            // only for turns that the caller classified as lacking substantive
            // progress, so equal gaps cannot join across productive turns.
            gap_fingerprint: effective_gap_fingerprint,
            usage_event: Some(GoalUsageEvent {
                usage_event_id: format!("{}:turn", active.context.outer_turn_id),
                goal_id: active.context.goal_id.clone(),
                source: "goal_outer_turn".to_string(),
                usage,
                created_at: finished_at,
            }),
            finished_at,
        })?;
        let ActiveGoalTurn {
            context, tracker, ..
        } = active;
        match action.clone() {
            GoalNextAction::Verify { intent: _ } => {
                self.pending_verification.insert(
                    session_id.to_string(),
                    PendingVerification { context, tracker },
                );
            }
            GoalNextAction::Pause {
                reason,
                ref message,
            } => {
                if requested_pause.is_none() {
                    self.store.transition_state(
                        &context.goal_id,
                        GoalState::Paused {
                            reason,
                            message: message.clone(),
                        },
                        "turn_paused",
                        Some(&context.outer_turn_id),
                        finished_at,
                    )?;
                }
            }
            GoalNextAction::BudgetLimited => {
                self.store.transition_state(
                    &context.goal_id,
                    GoalState::BudgetLimited,
                    "budget_limited",
                    Some(&context.outer_turn_id),
                    finished_at,
                )?;
            }
            _ => {
                self.trackers.insert(session_id.to_string(), tracker);
            }
        }
        Ok(action)
    }

    fn verify(
        &mut self,
        session_id: &str,
        result: GoalVerificationResult,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        let pending = self
            .pending_verification
            .remove(session_id)
            .ok_or_else(|| {
                GoalActorError::Invalid(
                    "no terminal goal intent is pending verification".to_string(),
                )
            })?;
        let PendingVerification {
            context,
            mut tracker,
        } = pending;
        let action = tracker.apply_verification(result).clone();
        match action.clone() {
            GoalNextAction::Complete { ref evidence } => self.store.transition_state(
                &context.goal_id,
                GoalState::Complete {
                    evidence: evidence.clone(),
                },
                "verified_complete",
                Some(&context.outer_turn_id),
                at,
            )?,
            GoalNextAction::Blocked { ref blocker } => self.store.transition_state(
                &context.goal_id,
                GoalState::Blocked {
                    blocker: blocker.clone(),
                },
                "verified_blocked",
                Some(&context.outer_turn_id),
                at,
            )?,
            GoalNextAction::Pause {
                reason,
                ref message,
            } => self.store.transition_state(
                &context.goal_id,
                GoalState::Paused {
                    reason,
                    message: message.clone(),
                },
                "verification_paused",
                Some(&context.outer_turn_id),
                at,
            )?,
            GoalNextAction::BudgetLimited => self.store.transition_state(
                &context.goal_id,
                GoalState::BudgetLimited,
                "budget_limited",
                Some(&context.outer_turn_id),
                at,
            )?,
            _ => {}
        }
        if !matches!(
            action,
            GoalNextAction::Complete { .. } | GoalNextAction::Blocked { .. }
        ) {
            self.trackers.insert(session_id.to_string(), tracker);
        }
        Ok(action)
    }

    fn pause(
        &mut self,
        session_id: &str,
        reason: orca_core::goal_runtime::GoalPauseReason,
        message: String,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        if let Some(active) = self.active.get_mut(session_id) {
            let next = GoalState::Paused {
                reason,
                message: message.clone(),
            };
            self.store.transition_state_while_turn_in_flight(
                &active.context.goal_id,
                next,
                "paused",
                &active.context.outer_turn_id,
                at,
            )?;
            active.pending_pause = Some(PendingGoalPause {
                reason,
                message: message.clone(),
            });
            active.tracker.pause(reason, message.clone());
            self.pending_verification.remove(session_id);
            return Ok(GoalNextAction::Pause { reason, message });
        }
        let record = self
            .store
            .get_by_session(session_id)?
            .ok_or_else(|| GoalActorError::Invalid("goal does not exist".to_string()))?;
        self.store.transition_state(
            &record.goal_id,
            GoalState::Paused {
                reason,
                message: message.clone(),
            },
            "paused",
            None,
            at,
        )?;
        self.trackers.remove(session_id);
        self.pending_verification.remove(session_id);
        Ok(GoalNextAction::Pause { reason, message })
    }

    fn resume(
        &mut self,
        session_id: &str,
        origin: GoalTurnOrigin,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        self.ensure_no_active_turn(session_id, "resume")?;
        let record = self
            .store
            .get_by_session(session_id)?
            .ok_or_else(|| GoalActorError::Invalid("goal does not exist".to_string()))?;
        let mut tracker = match self.trackers.remove(session_id) {
            Some(tracker) => tracker,
            None => {
                let history = self
                    .store
                    .recent_gap_fingerprints(&record.goal_id, SAME_GAP_STREAK_LIMIT)?;
                GoalTracker::from_record_with_history(&record, &history)
            }
        };
        let action = tracker.resume(origin).clone();
        if matches!(action, GoalNextAction::Continue { .. }) {
            self.store
                .transition_state(&record.goal_id, GoalState::Active, "resumed", None, at)?;
            self.trackers.insert(session_id.to_string(), tracker);
        }
        Ok(action)
    }
}

/// Builds the tracker input for a finished outer turn.
///
/// An explicit gap fingerprint always wins. Otherwise a gap is synthesized only
/// when the turn produced no observable activity at all — this was previously
/// unconditional, which made the tracker's progress branch unreachable and hid
/// genuinely stuck turns among productive ones.
fn build_turn_result(
    status: GoalTurnStatus,
    end_reason: crate::lifecycle::TurnEndReason,
    terminal: Option<orca_core::budget::OperationTerminal>,
    tool_count: u32,
    model_response_count: u32,
    has_substantive_progress: bool,
    gap_fingerprint: Option<String>,
) -> GoalTurnResult {
    // GoalTurnResult::evidence_count is usize; widen from the u32 wire counters.
    let activity_count = tool_count as usize + model_response_count as usize;
    let evidence_count = if gap_fingerprint.is_some() {
        0
    } else if has_substantive_progress {
        activity_count.max(1)
    } else {
        0
    };
    let gaps = match gap_fingerprint {
        Some(fingerprint) => vec![GoalGap {
            summary: "outer turn reported a structured gap".to_string(),
            fingerprint,
            model_fixable: true,
        }],
        None if !has_substantive_progress => vec![GoalGap {
            summary: "outer turn ended without structured progress evidence".to_string(),
            fingerprint: crate::goal_tracker::NO_SUBSTANTIVE_PROGRESS_GAP_FINGERPRINT.to_string(),
            model_fixable: true,
        }],
        None => Vec::new(),
    };
    GoalTurnResult {
        status,
        end_reason,
        terminal,
        usage: GoalUsage::default(),
        gaps,
        evidence_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::goal_runtime::{EvidenceItem, GoalPauseReason, GoalRequestedState, IntentId};
    use tempfile::tempdir;

    #[test]
    fn goal_actor_request_times_out_with_typed_error() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let timeout = std::time::Duration::from_millis(20);
        let (handle, join) = GoalRuntimeHandle::spawn_with_request_timeout_for_test(store, timeout);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (delay_reply_tx, delay_reply_rx) = mpsc::sync_channel(1);
        // Harness deadlines are liveness backstops for a stuck actor, not
        // latency assertions: under full-suite parallelism the actor thread
        // can be starved well past a second, so the backstops are generous
        // while still bounding a genuinely broken actor.
        let harness_backstop = std::time::Duration::from_secs(10);

        handle
            .sender
            .send(GoalActorCommand::DelayForTest {
                duration: std::time::Duration::from_millis(80),
                started: started_tx,
                reply: delay_reply_tx,
            })
            .unwrap();
        started_rx.recv_timeout(harness_backstop).unwrap();

        let started_at = std::time::Instant::now();
        let error = handle.latest_active().unwrap_err();
        assert!(matches!(
            error,
            GoalActorError::Timeout { timeout: actual } if actual == timeout
        ));
        assert!(
            started_at.elapsed() < harness_backstop,
            "goal actor request exceeded its bounded wait"
        );

        delay_reply_rx
            .recv_timeout(harness_backstop)
            .unwrap()
            .unwrap();
        // The actor has finished the delayed command; poll until it reports
        // an idle actor instead of racing its 20 ms request budget against
        // the scheduler under heavy test parallelism.
        let idle_deadline = std::time::Instant::now() + harness_backstop;
        loop {
            match handle.latest_active() {
                Ok(None) => break,
                Ok(Some(_)) => panic!("goal actor reported an active goal after the delay"),
                Err(GoalActorError::Timeout { .. })
                    if std::time::Instant::now() < idle_deadline =>
                {
                    std::thread::yield_now();
                }
                Err(error) => panic!("goal actor idle probe failed: {error:?}"),
            }
        }

        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn admitted_goal_mutation_waits_for_an_unambiguous_result() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let timeout = std::time::Duration::from_millis(20);
        let (handle, join) = GoalRuntimeHandle::spawn_with_request_timeout_for_test(store, timeout);
        let blocker = handle
            .delay_for_test(std::time::Duration::from_millis(80))
            .unwrap();

        let started_at = std::time::Instant::now();
        let record = create(&handle, "mutation-waits");

        assert_eq!(record.session_id, "mutation-waits");
        assert!(started_at.elapsed() >= std::time::Duration::from_millis(40));
        blocker.join().unwrap();
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    fn create(handle: &GoalRuntimeHandle, session_id: &str) -> GoalRecord {
        handle
            .create(CreateGoalInput {
                session_id: session_id.to_string(),
                objective: "actor-owned goal".to_string(),
                token_budget: None,
                now: 1,
            })
            .unwrap()
    }

    fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
        let mut bytes = [seed; 16];
        bytes[6] = 0x70 | (seed & 0x0f);
        bytes[8] = 0x80 | (seed & 0x3f);
        bytes
    }

    fn test_surface_goal_identity(
        goal_id: &GoalId,
        goal_run_id: &orca_core::goal_runtime::GoalRunId,
    ) -> crate::runtime_surface::SurfaceGoalGenerationIdentity {
        let operation_fence = crate::runtime_surface::SurfaceOperationFence {
            thread_id: crate::runtime_surface::SurfaceThreadId::try_from_bytes(uuid_v7_bytes(21))
                .unwrap(),
            thread_owner_epoch: crate::runtime_surface::ThreadOwnerEpoch::new(1),
            operation_id: crate::runtime_surface::SurfaceOperationId::try_from_bytes(
                uuid_v7_bytes(22),
            )
            .unwrap(),
            generation_id: crate::runtime_surface::SurfaceGenerationId::new(0),
        };
        crate::runtime_surface::SurfaceGoalGenerationIdentity {
            goal_id: crate::runtime_surface::SurfaceGoalId::try_new(goal_id.to_string()).unwrap(),
            goal_run_id: crate::runtime_surface::SurfaceGoalRunId::try_new(goal_run_id.to_string())
                .unwrap(),
            operation_fence,
            goal_outer_turn_id: crate::runtime_surface::SurfaceGoalOuterTurnId::try_new(
                GoalOuterTurnId::new().to_string(),
            )
            .unwrap(),
            logical_turn_id: orca_core::thread_identity::TurnId::new(),
            canonical_input_item_id: orca_core::thread_identity::ConversationItemId::new(),
            outer_turn_origin: crate::runtime_surface::GoalOuterTurnOrigin::User,
            attempt: crate::runtime_surface::GenerationAttempt::Initial,
            predecessor_fence: None,
            objective_revision: crate::runtime_surface::GoalObjectiveRevision::new(1),
            outer_turn_count: 1,
        }
    }

    #[test]
    fn outer_turn_result_reflects_evidence_instead_of_constant_gap() {
        let active = build_turn_result(
            GoalTurnStatus::Success,
            crate::lifecycle::TurnEndReason::Unclassified,
            None,
            4,
            2,
            true,
            None,
        );
        assert_eq!(active.evidence_count, 6);
        assert!(
            active.gaps.is_empty(),
            "a turn with activity and no explicit gap must not synthesize one"
        );

        let idle = build_turn_result(
            GoalTurnStatus::Success,
            crate::lifecycle::TurnEndReason::Unclassified,
            None,
            0,
            0,
            false,
            None,
        );
        assert_eq!(idle.evidence_count, 0);
        assert_eq!(idle.gaps.len(), 1);
        assert_eq!(
            idle.gaps[0].fingerprint,
            crate::goal_tracker::NO_SUBSTANTIVE_PROGRESS_GAP_FINGERPRINT
        );

        let plan_only = build_turn_result(
            GoalTurnStatus::Success,
            crate::lifecycle::TurnEndReason::Unclassified,
            None,
            0,
            0,
            true,
            None,
        );
        assert_eq!(plan_only.evidence_count, 1);
        assert!(
            plan_only.gaps.is_empty(),
            "a changed plan is substantive progress even without new messages"
        );

        let explicit = build_turn_result(
            GoalTurnStatus::Success,
            crate::lifecycle::TurnEndReason::Unclassified,
            None,
            5,
            1,
            false,
            Some("roadmap:next-slice".to_string()),
        );
        assert_eq!(explicit.gaps.len(), 1);
        assert_eq!(explicit.gaps[0].fingerprint, "roadmap:next-slice");
        assert_eq!(
            explicit.evidence_count, 0,
            "an explicit no-progress gap must not be erased by read/tool activity"
        );

        let exploratory = build_turn_result(
            GoalTurnStatus::Success,
            crate::lifecycle::TurnEndReason::Unclassified,
            None,
            7,
            3,
            false,
            Some("outer_turn:no_substantive_progress".to_string()),
        );
        assert_eq!(exploratory.evidence_count, 0);
    }

    #[test]
    fn goal_runtime_lease_child_probe() {
        let Ok(path) = std::env::var("ORCA_TEST_GOAL_RUNTIME_LEASE_PATH") else {
            return;
        };
        let store = GoalStore::open(&path).unwrap();
        assert!(matches!(
            GoalRuntimeLease::acquire(&store),
            Err(GoalActorError::OwnerActive { .. })
        ));
    }

    #[test]
    fn goal_runtime_lease_is_shared_in_process_and_exclusive_across_processes() {
        let dir = tempdir().unwrap();
        let database_path = dir.path().join("goals.sqlite3");
        let store = GoalStore::open(&database_path).unwrap();
        let (first, first_in_process) = GoalRuntimeLease::acquire(&store).unwrap();
        let (second, second_in_process) = GoalRuntimeLease::acquire(&store).unwrap();
        assert!(first_in_process);
        assert!(!second_in_process);

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "goal_actor::tests::goal_runtime_lease_child_probe",
                "--nocapture",
            ])
            .env("ORCA_TEST_GOAL_RUNTIME_LEASE_PATH", &database_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child lease probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        drop(first);
        let (third, third_in_process) = GoalRuntimeLease::acquire(&store).unwrap();
        assert!(
            !third_in_process,
            "second in-process lease still owns the lock"
        );
        drop(second);
        drop(third);
        let (_fourth, fourth_in_process) = GoalRuntimeLease::acquire(&store).unwrap();
        assert!(fourth_in_process);
    }

    #[test]
    fn mailbox_returns_one_reply_and_owns_goal_lifecycle() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let (handle, join) = GoalRuntimeHandle::spawn(store);
        let goal = create(&handle, "actor-session");
        let _turn = handle
            .begin_outer_turn("actor-session", GoalTurnOrigin::User, "provider-1", 2)
            .unwrap();
        let intent = GoalUpdateIntent {
            intent_id: IntentId::new(),
            requested_state: GoalRequestedState::Complete,
            reason: "verified by tests".to_string(),
            evidence: vec![EvidenceItem::observation("test passed")],
            blocker: None,
        };
        let ack = handle.submit_intent("actor-session", intent, 3).unwrap();
        assert!(matches!(ack, GoalUpdateAck::DeferredToTurnEnd { .. }));
        let action = handle
            .finish_outer_turn_with_progress(
                "actor-session",
                GoalTurnStatus::Success,
                crate::lifecycle::TurnEndReason::Unclassified,
                None,
                GoalUsage::default(),
                1,
                1,
                true,
                None,
                4,
            )
            .unwrap();
        assert!(matches!(action, GoalNextAction::Verify { .. }));
        let action = handle
            .verify(
                "actor-session",
                GoalVerificationResult::Achieved {
                    evidence: vec![EvidenceItem::observation("verified")],
                },
                5,
            )
            .unwrap();
        assert!(matches!(action, GoalNextAction::Complete { .. }));
        let record = handle.read("actor-session").unwrap().unwrap();
        assert_eq!(record.goal_id, goal.goal_id);
        assert!(matches!(record.state, GoalState::Complete { .. }));
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn surface_receipts_and_outbox_settlement_remain_actor_owned() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let (handle, join) = GoalRuntimeHandle::spawn_surface_owned_for_test(store);
        let context = crate::goal_store::GoalSurfaceMutationContext {
            store_commit_id: "019f8b4d-7d73-7b52-8f44-2cfeac060007".to_string(),
            command_digest: [8; 32],
            goal_owner_epoch: 1,
        };
        let created = handle
            .create_for_surface(
                CreateGoalInput {
                    session_id: "actor-surface".to_string(),
                    objective: "keep the durable receipt behind the actor".to_string(),
                    token_budget: None,
                    now: 100,
                },
                context.clone(),
            )
            .unwrap();

        assert_eq!(
            handle.pending_surface_mutations("actor-surface").unwrap(),
            vec![created.clone()]
        );
        assert!(
            handle
                .acknowledge_surface_mutation(
                    &context.store_commit_id,
                    &created.receipt.receipt_digest,
                )
                .unwrap()
        );
        assert!(
            handle
                .pending_surface_mutations("actor-surface")
                .unwrap()
                .is_empty()
        );

        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn actor_without_runtime_lease_cannot_acknowledge_surface_recovery_work() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let context = crate::goal_store::GoalSurfaceMutationContext {
            store_commit_id: "019f8b4d-7d73-7b52-8f44-2cfeac060008".to_string(),
            command_digest: [9; 32],
            goal_owner_epoch: owner_epoch,
        };
        let created = store
            .create_goal_for_surface(
                CreateGoalInput {
                    session_id: "unleased-actor-ack".to_string(),
                    objective: "retain recovery until the runtime owner commits it".to_string(),
                    token_budget: None,
                    now: 100,
                },
                context,
            )
            .unwrap();
        let (handle, join) = GoalRuntimeHandle::spawn(store);

        let error = handle
            .acknowledge_surface_mutation(
                &created.receipt.store_commit_id,
                &created.receipt.receipt_digest,
            )
            .unwrap_err();

        assert!(error.to_string().contains("durable runtime owner lease"));
        assert_eq!(
            handle
                .pending_surface_mutations("unleased-actor-ack")
                .unwrap(),
            vec![created]
        );
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn max_inner_surface_continuation_without_preview_is_rejected_before_store() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let inspection_store = store.clone();
        let (handle, join) = GoalRuntimeHandle::spawn_surface_owned_for_test(store);
        let goal_id = GoalId::new();
        let goal_run_id = orca_core::goal_runtime::GoalRunId::new();
        let predecessor = test_surface_goal_identity(&goal_id, &goal_run_id);
        let mut successor = predecessor.clone();
        successor.operation_fence.generation_id =
            crate::runtime_surface::SurfaceGenerationId::new(1);
        successor.goal_outer_turn_id = crate::runtime_surface::SurfaceGoalOuterTurnId::try_new(
            GoalOuterTurnId::new().to_string(),
        )
        .unwrap();
        successor.logical_turn_id = orca_core::thread_identity::TurnId::new();
        successor.canonical_input_item_id = orca_core::thread_identity::ConversationItemId::new();
        successor.outer_turn_origin = crate::runtime_surface::GoalOuterTurnOrigin::Continuation;
        successor.predecessor_fence = Some(predecessor.operation_fence.clone());
        successor.outer_turn_count = 2;
        let terminal = crate::runtime_surface::OperationTerminal::BudgetExhausted {
            budget: crate::runtime_surface::OperationBudget::TurnRequests {
                scope: crate::runtime_surface::TurnRequestBudgetScope::AgentLoop,
                // No fixed ceiling exists anymore; the test only needs a
                // positive limit/observed pair for the surface contract.
                limit: 128,
                observed: 128,
            },
        };
        let before_goal_count = inspection_store.goal_count().unwrap();
        let before_in_flight = inspection_store.in_flight_run_count().unwrap();
        let before_outbox = inspection_store
            .pending_surface_mutations("missing-preview")
            .unwrap();

        let error = handle
            .finish_outer_turn_for_surface(
                FinishGoalOuterTurnForSurfaceInput {
                    session_id: "missing-preview".to_string(),
                    expected_goal_id: goal_id,
                    expected_goal_revision: 1,
                    identity: Box::new(predecessor),
                    status: crate::runtime_surface::GoalOuterTurnStatus::BudgetExhausted,
                    usage: crate::runtime_surface::GoalUsage {
                        charged_input_tokens: 0,
                        output_tokens: 0,
                        cache_tokens: 0,
                        verifier_tokens: 0,
                        cost_micros: 0,
                        elapsed_seconds: 0,
                    },
                    progress: GoalSurfaceTurnProgress {
                        tool_count: 0,
                        model_response_count: 0,
                        gap_fingerprint: Some(
                            crate::goal_tracker::NO_SUBSTANTIVE_PROGRESS_GAP_FINGERPRINT
                                .to_string(),
                        ),
                    },
                    next_action: crate::runtime_surface::GoalOuterTurnNextAction::Continue,
                    verification: None,
                    continuation: Some(crate::goal_store::AdmittedGoalContinuationForSurface {
                        reason: crate::runtime_surface::GoalContinuationAdmitReason::GapFeedback,
                        successor: Box::new(successor),
                        provider_turn_id: "provider-must-not-start".to_string(),
                    }),
                    stop_reason: crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                        state: crate::runtime_surface::SurfaceGoalState::Active,
                    },
                    terminal,
                    pause_message: "missing preview must reject".to_string(),
                    finished_at: 2,
                },
                vec![
                    GoalSurfaceMutationContext {
                        store_commit_id: "019f8b4d-7d73-7b52-8f44-2cfeac060091".to_string(),
                        command_digest: [91; 32],
                        goal_owner_epoch: 1,
                    },
                    GoalSurfaceMutationContext {
                        store_commit_id: "019f8b4d-7d73-7b52-8f44-2cfeac060092".to_string(),
                        command_digest: [92; 32],
                        goal_owner_epoch: 1,
                    },
                ],
            )
            .unwrap_err();

        assert!(error.to_string().contains("previewed decision"));
        assert_eq!(inspection_store.goal_count().unwrap(), before_goal_count);
        assert_eq!(
            inspection_store.in_flight_run_count().unwrap(),
            before_in_flight
        );
        assert_eq!(
            inspection_store
                .pending_surface_mutations("missing-preview")
                .unwrap(),
            before_outbox
        );
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn surface_finish_errors_preserve_the_preview_owner_token() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let record = store
            .create_goal(CreateGoalInput {
                session_id: "preview-retry".to_string(),
                objective: "retain the exact preview owner across retry".to_string(),
                token_budget: None,
                now: 1,
            })
            .unwrap();
        let goal_run_id = orca_core::goal_runtime::GoalRunId::new();
        let identity = test_surface_goal_identity(&record.goal_id, &goal_run_id);
        let progress = GoalSurfaceTurnProgress {
            tool_count: 0,
            model_response_count: 0,
            gap_fingerprint: Some(
                crate::goal_tracker::NO_SUBSTANTIVE_PROGRESS_GAP_FINGERPRINT.to_string(),
            ),
        };
        let pause_message = "pause after no progress".to_string();
        let input = FinishGoalOuterTurnForSurfaceInput {
            session_id: record.session_id.clone(),
            expected_goal_id: record.goal_id.clone(),
            expected_goal_revision: 1,
            identity: Box::new(identity.clone()),
            status: crate::runtime_surface::GoalOuterTurnStatus::Success,
            usage: crate::runtime_surface::GoalUsage {
                charged_input_tokens: 0,
                output_tokens: 0,
                cache_tokens: 0,
                verifier_tokens: 0,
                cost_micros: 0,
                elapsed_seconds: 0,
            },
            progress: progress.clone(),
            next_action: crate::runtime_surface::GoalOuterTurnNextAction::Pause,
            verification: None,
            continuation: None,
            stop_reason: crate::runtime_surface::GoalContinuationStopReason::GoalInactive {
                state: crate::runtime_surface::SurfaceGoalState::Paused {
                    reason: crate::runtime_surface::SurfaceGoalPauseReason::NoProgress,
                    message: crate::runtime_surface::DisplayText::new(&pause_message),
                },
            },
            terminal: crate::runtime_surface::OperationTerminal::Succeeded {
                usage: crate::runtime_surface::UsageTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_tokens: 0,
                    estimated_cost_usd_micros: 0,
                },
            },
            pause_message: pause_message.clone(),
            finished_at: 2,
        };
        let (_sender, receiver) = mpsc::sync_channel(ACTOR_MAILBOX_CAPACITY);
        let mut actor = GoalActor {
            store,
            sender: receiver,
            active: HashMap::new(),
            trackers: HashMap::new(),
            pending_verification: HashMap::new(),
            pending_surface_decisions: HashMap::from([(
                record.session_id.clone(),
                PendingSurfaceDecision {
                    identity: Box::new(identity),
                    status: GoalTurnStatus::Success,
                    usage: GoalUsage::default(),
                    verification: None,
                    action: GoalNextAction::Pause {
                        reason: GoalPauseReason::NoProgress,
                        message: pause_message,
                    },
                    tracker: GoalTracker::from_record(&record),
                    progress,
                },
            )]),
            pending_recoveries: HashMap::new(),
            surface_owner_epoch: Some(owner_epoch),
            _runtime_lease: None,
        };
        let contexts = || {
            vec![
                GoalSurfaceMutationContext {
                    store_commit_id: uuid::Uuid::now_v7().to_string(),
                    command_digest: [31; 32],
                    goal_owner_epoch: owner_epoch,
                },
                GoalSurfaceMutationContext {
                    store_commit_id: uuid::Uuid::now_v7().to_string(),
                    command_digest: [32; 32],
                    goal_owner_epoch: owner_epoch,
                },
            ]
        };

        let mut mismatched = input.clone();
        mismatched.progress.tool_count = 1;
        let (reply, result) = mpsc::sync_channel(1);
        actor.handle(GoalActorCommand::FinishOuterTurnForSurface {
            input: mismatched,
            contexts: contexts(),
            reply,
        });
        assert!(matches!(
            result.recv().unwrap(),
            Err(GoalActorError::Invalid(message)) if message.contains("differs")
        ));
        assert!(
            actor
                .pending_surface_decisions
                .contains_key("preview-retry")
        );

        inject_surface_outer_turn_finish_failure_once("preview-retry");
        let (reply, result) = mpsc::sync_channel(1);
        actor.handle(GoalActorCommand::FinishOuterTurnForSurface {
            input,
            contexts: contexts(),
            reply,
        });
        assert!(matches!(
            result.recv().unwrap(),
            Err(GoalActorError::Store(message)) if message.contains("injected")
        ));
        assert!(
            actor
                .pending_surface_decisions
                .contains_key("preview-retry"),
            "a failed durable write must return the exact preview token to its actor owner"
        );
    }

    #[test]
    fn duplicate_intent_is_idempotent_and_stale_turn_is_rejected() {
        let dir = tempdir().unwrap();
        let (handle, join) =
            GoalRuntimeHandle::spawn(GoalStore::open(dir.path().join("goals.sqlite3")).unwrap());
        create(&handle, "duplicate-session");
        handle
            .begin_outer_turn("duplicate-session", GoalTurnOrigin::User, "provider-1", 1)
            .unwrap();
        let intent = GoalUpdateIntent {
            intent_id: IntentId::new(),
            requested_state: GoalRequestedState::Complete,
            reason: "done".to_string(),
            evidence: vec![EvidenceItem::observation("proof")],
            blocker: None,
        };
        let first = handle
            .submit_intent("duplicate-session", intent.clone(), 2)
            .unwrap();
        let second = handle
            .submit_intent("duplicate-session", intent, 2)
            .unwrap();
        assert!(matches!(first, GoalUpdateAck::DeferredToTurnEnd { .. }));
        assert!(matches!(second, GoalUpdateAck::AlreadyPending { .. }));
        let error = handle
            .begin_outer_turn(
                "duplicate-session",
                GoalTurnOrigin::Continuation,
                "provider-2",
                3,
            )
            .unwrap_err();
        assert!(error.to_string().contains("active outer turn"));
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn actor_records_verifier_usage_against_closed_outer_turn_once() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let inspection_store = store.clone();
        let (handle, join) = GoalRuntimeHandle::spawn(store);
        let goal = create(&handle, "verifier-usage-session");
        let turn = handle
            .begin_outer_turn(
                "verifier-usage-session",
                GoalTurnOrigin::User,
                "provider-verifier-usage",
                1,
            )
            .unwrap();
        handle
            .finish_outer_turn_with_progress(
                "verifier-usage-session",
                GoalTurnStatus::Success,
                crate::lifecycle::TurnEndReason::Unclassified,
                None,
                GoalUsage::default(),
                1,
                1,
                true,
                None,
                2,
            )
            .unwrap();
        let event = GoalUsageEvent {
            usage_event_id: format!("verifier:{}:1", turn.outer_turn_id),
            goal_id: goal.goal_id,
            source: "goal_verifier".to_string(),
            usage: GoalUsage {
                verifier_tokens: 23,
                ..GoalUsage::default()
            },
            created_at: 3,
        };

        let first = handle
            .record_verifier_usage_once(&turn.outer_turn_id, event.clone())
            .unwrap();
        let second = handle
            .record_verifier_usage_once(&turn.outer_turn_id, event)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(first.verifier_tokens, 23);
        assert_eq!(
            inspection_store
                .outer_turn_verifier_tokens(&turn.outer_turn_id)
                .unwrap(),
            Some(23)
        );
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn pause_waits_for_active_turn_settlement_then_resume_starts_a_fresh_run() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let inspection_store = store.clone();
        let (handle, join) = GoalRuntimeHandle::spawn(store);
        create(&handle, "pause-resume-session");
        let first = handle
            .begin_outer_turn(
                "pause-resume-session",
                GoalTurnOrigin::User,
                "provider-before-pause",
                1,
            )
            .unwrap();

        handle
            .pause(
                "pause-resume-session",
                GoalPauseReason::User,
                "user paused",
                2,
            )
            .unwrap();
        assert_eq!(
            inspection_store
                .outer_turn_status(&first.outer_turn_id)
                .unwrap()
                .as_deref(),
            Some("in_flight")
        );
        assert!(matches!(
            handle
                .finish_outer_turn_with_progress(
                    "pause-resume-session",
                    GoalTurnStatus::Cancelled,
                    crate::lifecycle::TurnEndReason::Cancelled,
                    None,
                    GoalUsage {
                        charged_input_tokens: 5,
                        output_tokens: 2,
                        ..GoalUsage::default()
                    },
                    0,
                    0,
                    false,
                    None,
                    3,
                )
                .unwrap(),
            GoalNextAction::Pause {
                reason: GoalPauseReason::User,
                ..
            }
        ));
        handle
            .resume("pause-resume-session", GoalTurnOrigin::Resume, 4)
            .unwrap();
        let resumed = handle
            .begin_outer_turn(
                "pause-resume-session",
                GoalTurnOrigin::Resume,
                "provider-after-resume",
                5,
            )
            .unwrap();

        assert_eq!(
            inspection_store
                .outer_turn_status(&first.outer_turn_id)
                .unwrap()
                .as_deref(),
            Some("cancelled")
        );
        assert_ne!(resumed.goal_run_id, first.goal_run_id);
        assert!(resumed.run_started);
        assert_eq!(resumed.origin, GoalTurnOrigin::Resume);
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn rejected_active_controls_do_not_discard_actor_turn_ownership() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let (handle, join) = GoalRuntimeHandle::spawn(store);
        create(&handle, "active-control-session");
        handle
            .begin_outer_turn(
                "active-control-session",
                GoalTurnOrigin::User,
                "active-control-provider-turn",
                1,
            )
            .unwrap();

        assert!(matches!(
            handle.edit(
                "active-control-session",
                "must wait for cancellation",
                None,
                2,
            ),
            Err(GoalActorError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            handle.resume("active-control-session", GoalTurnOrigin::Resume, 3),
            Err(GoalActorError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            handle.clear("active-control-session"),
            Err(GoalActorError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            handle
                .continuation_state("active-control-session")
                .unwrap()
                .unwrap()
                .status,
            GoalContinuationStatus::OuterTurnInFlight
        ));
        assert!(matches!(
            handle
                .finish_outer_turn_with_progress(
                    "active-control-session",
                    GoalTurnStatus::Cancelled,
                    crate::lifecycle::TurnEndReason::Cancelled,
                    None,
                    GoalUsage::default(),
                    0,
                    0,
                    false,
                    None,
                    4,
                )
                .unwrap(),
            GoalNextAction::Pause {
                reason: GoalPauseReason::Infrastructure,
                ..
            }
        ));

        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn same_gap_streak_survives_two_actor_restarts() {
        let dir = tempdir().unwrap();
        let database_path = dir.path().join("goals.sqlite3");
        let store = GoalStore::open(&database_path).unwrap();
        let (handle, join) = GoalRuntimeHandle::spawn(store);
        create(&handle, "streak-restart-session");

        handle
            .begin_outer_turn(
                "streak-restart-session",
                GoalTurnOrigin::User,
                "provider-1",
                1,
            )
            .unwrap();
        assert!(matches!(
            handle
                .finish_outer_turn_with_progress(
                    "streak-restart-session",
                    GoalTurnStatus::Success,
                    crate::lifecycle::TurnEndReason::Unclassified,
                    None,
                    GoalUsage::default(),
                    0,
                    0,
                    false,
                    Some("gap:repeat".to_string()),
                    2,
                )
                .unwrap(),
            GoalNextAction::Continue { .. }
        ));
        handle.shutdown().unwrap();
        join.join().unwrap();
        drop(handle);

        let (handle, join) = GoalRuntimeHandle::spawn(GoalStore::open(&database_path).unwrap());
        handle
            .begin_outer_turn(
                "streak-restart-session",
                GoalTurnOrigin::Continuation,
                "provider-2",
                3,
            )
            .unwrap();
        assert!(matches!(
            handle
                .finish_outer_turn_with_progress(
                    "streak-restart-session",
                    GoalTurnStatus::Success,
                    crate::lifecycle::TurnEndReason::Unclassified,
                    None,
                    GoalUsage::default(),
                    0,
                    0,
                    false,
                    Some("gap:repeat".to_string()),
                    4,
                )
                .unwrap(),
            GoalNextAction::Continue { .. }
        ));
        handle.shutdown().unwrap();
        join.join().unwrap();
        drop(handle);

        let (handle, join) = GoalRuntimeHandle::spawn(GoalStore::open(&database_path).unwrap());
        handle
            .begin_outer_turn(
                "streak-restart-session",
                GoalTurnOrigin::Continuation,
                "provider-3",
                5,
            )
            .unwrap();
        let action = handle
            .finish_outer_turn_with_progress(
                "streak-restart-session",
                GoalTurnStatus::Success,
                crate::lifecycle::TurnEndReason::Unclassified,
                None,
                GoalUsage::default(),
                0,
                0,
                false,
                Some("gap:repeat".to_string()),
                6,
            )
            .unwrap();
        assert!(matches!(
            action,
            GoalNextAction::Pause {
                reason: GoalPauseReason::NoProgress,
                ..
            }
        ));

        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn substantive_progress_is_a_restart_barrier_for_same_gap_streak() {
        let dir = tempdir().unwrap();
        let database_path = dir.path().join("goals.sqlite3");
        let (handle, join) = GoalRuntimeHandle::spawn(GoalStore::open(&database_path).unwrap());
        create(&handle, "progress-barrier-session");
        handle
            .begin_outer_turn(
                "progress-barrier-session",
                GoalTurnOrigin::User,
                "provider-gap-before-progress",
                1,
            )
            .unwrap();
        assert!(matches!(
            handle
                .finish_outer_turn_with_progress(
                    "progress-barrier-session",
                    GoalTurnStatus::Success,
                    crate::lifecycle::TurnEndReason::Unclassified,
                    None,
                    GoalUsage::default(),
                    0,
                    0,
                    false,
                    Some("gap:repeat".to_string()),
                    2,
                )
                .unwrap(),
            GoalNextAction::Continue { .. }
        ));
        handle.shutdown().unwrap();
        join.join().unwrap();
        drop(handle);

        let (handle, join) = GoalRuntimeHandle::spawn(GoalStore::open(&database_path).unwrap());
        handle
            .begin_outer_turn(
                "progress-barrier-session",
                GoalTurnOrigin::Continuation,
                "provider-plan-only-progress",
                3,
            )
            .unwrap();
        assert!(matches!(
            handle
                .finish_outer_turn_with_progress(
                    "progress-barrier-session",
                    GoalTurnStatus::Success,
                    crate::lifecycle::TurnEndReason::Unclassified,
                    None,
                    GoalUsage::default(),
                    0,
                    0,
                    true,
                    None,
                    4,
                )
                .unwrap(),
            GoalNextAction::Continue { .. }
        ));
        handle.shutdown().unwrap();
        join.join().unwrap();
        drop(handle);

        let (handle, join) = GoalRuntimeHandle::spawn(GoalStore::open(&database_path).unwrap());
        handle
            .begin_outer_turn(
                "progress-barrier-session",
                GoalTurnOrigin::Continuation,
                "provider-gap-after-progress",
                5,
            )
            .unwrap();
        assert!(matches!(
            handle
                .finish_outer_turn_with_progress(
                    "progress-barrier-session",
                    GoalTurnStatus::Success,
                    crate::lifecycle::TurnEndReason::Unclassified,
                    None,
                    GoalUsage::default(),
                    0,
                    0,
                    false,
                    Some("gap:repeat".to_string()),
                    6,
                )
                .unwrap(),
            GoalNextAction::Continue { .. }
        ));
        handle.shutdown().unwrap();
        join.join().unwrap();
    }
}
