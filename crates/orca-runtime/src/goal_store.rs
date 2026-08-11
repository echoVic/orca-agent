use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use orca_core::goal_runtime::{
    BlockerKind, BlockerSummary, GoalId, GoalOuterTurnId, GoalPauseReason, GoalRecord,
    GoalRequestedState, GoalRunId, GoalRunSnapshot, GoalState, GoalTransitionSummary,
    GoalTurnOrigin, GoalTurnStatus, GoalUpdateAck, GoalUpdateIntent, GoalUsage,
};
use orca_core::goal_types::{ThreadGoal, ThreadGoalStatus, validate_thread_goal_objective};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runtime_surface::{
    GenerationAttempt, OperationKind, OperationPhase, OperationRecord,
    SurfaceGoalGenerationIdentity, SurfaceOperationId,
};
use orca_platform::fs::ExclusiveFileLock;

const SCHEMA_VERSION: i64 = 4;
const DATABASE_FILENAME: &str = "goals.sqlite3";
const LEGACY_FILENAME: &str = "goals_1.json";
const LEGACY_MIGRATION_KEY: &str = "legacy_goals_1_migrated";

#[derive(Clone, Debug)]
pub struct GoalStore {
    path: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GoalAuditSnapshot {
    pub outer_turns: i64,
    pub intents: i64,
    pub usage_events: i64,
    pub verifier_tokens: i64,
    pub transitions: i64,
    pub in_flight_runs: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalRecoveryRecord {
    pub session_id: String,
    pub goal_id: GoalId,
    pub stale_goal_run_id: GoalRunId,
    pub outer_turn_id: Option<GoalOuterTurnId>,
    pub recovered_state: GoalState,
}

#[derive(Debug)]
pub enum GoalStoreError {
    Sqlite(rusqlite::Error),
    Io(io::Error),
    Json(serde_json::Error),
    Invalid(String),
    Migration(String),
}

impl fmt::Display for GoalStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "goal database error: {error}"),
            Self::Io(error) => write!(formatter, "goal database I/O error: {error}"),
            Self::Json(error) => write!(formatter, "goal database JSON error: {error}"),
            Self::Invalid(message) => formatter.write_str(message),
            Self::Migration(message) => {
                write!(formatter, "legacy goal migration failed: {message}")
            }
        }
    }
}

impl std::error::Error for GoalStoreError {}

impl From<rusqlite::Error> for GoalStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<io::Error> for GoalStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for GoalStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateGoalInput {
    pub session_id: String,
    pub objective: String,
    pub token_budget: Option<i64>,
    pub now: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeginGoalRunInput {
    pub goal_id: GoalId,
    pub goal_run_id: GoalRunId,
    pub origin: GoalTurnOrigin,
    pub started_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeginOuterTurnInput {
    pub goal_id: GoalId,
    pub goal_run_id: GoalRunId,
    pub outer_turn_id: GoalOuterTurnId,
    pub origin: GoalTurnOrigin,
    pub provider_turn_id: String,
    pub started_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalUsageEvent {
    pub usage_event_id: String,
    pub goal_id: GoalId,
    pub source: String,
    pub usage: GoalUsage,
    pub created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalIntentRecord {
    pub outer_turn_id: GoalOuterTurnId,
    pub intent: GoalUpdateIntent,
    pub ack: GoalUpdateAck,
    pub created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishOuterTurnInput {
    pub goal_id: GoalId,
    pub goal_run_id: GoalRunId,
    pub outer_turn_id: GoalOuterTurnId,
    pub status: orca_core::goal_runtime::GoalTurnStatus,
    pub tool_count: u32,
    pub model_response_count: u32,
    pub gap_fingerprint: Option<String>,
    pub usage_event: Option<GoalUsageEvent>,
    pub finished_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishOuterTurnOutcome {
    pub already_finished: bool,
    pub usage: GoalUsage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalSurfaceMutationContext {
    pub store_commit_id: String,
    pub command_digest: [u8; 32],
    pub goal_owner_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareGoalRunForSurfaceInput {
    pub session_id: String,
    pub expected_goal_id: GoalId,
    pub expected_goal_revision: u32,
    pub expected_receipt_digest: [u8; 32],
    pub goal_run_id: GoalRunId,
    pub operation: Box<OperationRecord>,
    pub origin: GoalTurnOrigin,
    pub started_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateGoalAndPrepareRunForSurfaceInput {
    pub goal: CreateGoalInput,
    pub goal_id: GoalId,
    pub goal_run_id: GoalRunId,
    pub operation: Box<OperationRecord>,
    pub origin: GoalTurnOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditGoalAndPrepareRunForSurfaceInput {
    pub session_id: String,
    pub expected_goal_id: GoalId,
    pub expected_goal_revision: u32,
    pub expected_receipt_digest: [u8; 32],
    pub objective: String,
    pub token_budget: Option<i64>,
    pub goal_run_id: GoalRunId,
    pub operation: Box<OperationRecord>,
    pub origin: GoalTurnOrigin,
    pub started_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeginGoalOuterTurnForSurfaceInput {
    pub session_id: String,
    pub expected_goal_id: GoalId,
    pub expected_goal_revision: u32,
    pub identity: Box<SurfaceGoalGenerationIdentity>,
    pub provider_turn_id: String,
    pub started_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishGoalOuterTurnForSurfaceInput {
    pub session_id: String,
    pub expected_goal_id: GoalId,
    pub expected_goal_revision: u32,
    pub identity: Box<SurfaceGoalGenerationIdentity>,
    pub status: crate::runtime_surface::GoalOuterTurnStatus,
    pub usage: crate::runtime_surface::GoalUsage,
    pub progress: GoalSurfaceTurnProgress,
    pub next_action: crate::runtime_surface::GoalOuterTurnNextAction,
    pub verification: Option<crate::runtime_surface::SurfaceGoalVerification>,
    pub continuation: Option<AdmittedGoalContinuationForSurface>,
    pub stop_reason: crate::runtime_surface::GoalContinuationStopReason,
    pub terminal: crate::runtime_surface::OperationTerminal,
    pub pause_message: String,
    pub finished_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalSurfaceTurnProgress {
    pub tool_count: u32,
    pub model_response_count: u32,
    pub gap_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedGoalContinuationForSurface {
    pub reason: crate::runtime_surface::GoalContinuationAdmitReason,
    pub successor: Box<SurfaceGoalGenerationIdentity>,
    pub provider_turn_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseGoalForSurfaceInput {
    pub session_id: String,
    pub expected_goal_id: GoalId,
    pub expected_goal_revision: u32,
    pub expected_operation_id: SurfaceOperationId,
    pub message: String,
    pub paused_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseQuiescentGoalForSurfaceInput {
    pub session_id: String,
    pub expected_goal_id: GoalId,
    pub expected_goal_revision: u32,
    pub message: String,
    pub paused_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoverGoalRunForSurfaceInput {
    pub session_id: String,
    pub expected_goal_id: GoalId,
    pub expected_goal_revision: u32,
    pub stale_identity: Option<Box<SurfaceGoalGenerationIdentity>>,
    pub recovery_message: String,
    pub recovered_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaceGoalContinuationForSurfaceInput {
    pub interrupted: GoalSurfaceMutationRecord,
    pub surface_previous_revision: u32,
    pub stale_run_settled: bool,
    pub recovery_message: String,
    pub recovered_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalSurfaceTokenBudgetUpdate {
    Keep,
    Set(Option<i64>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum GoalSurfaceMutation {
    Created,
    CreatedWithRun {
        goal_run_id: GoalRunId,
        operation: Box<OperationRecord>,
        origin: GoalTurnOrigin,
    },
    Edited {
        previous_revision: u32,
    },
    Removed {
        previous_revision: u32,
        tombstone_revision: u32,
    },
    RunStarted {
        previous_revision: u32,
        goal_run_id: GoalRunId,
        operation: Box<OperationRecord>,
        origin: GoalTurnOrigin,
    },
    OuterTurnStarted {
        previous_revision: u32,
        identity: Box<SurfaceGoalGenerationIdentity>,
    },
    IntentRequested {
        previous_revision: u32,
        identity: Box<SurfaceGoalGenerationIdentity>,
        intent: GoalUpdateIntent,
    },
    IntentAcknowledged {
        previous_revision: u32,
        identity: Box<SurfaceGoalGenerationIdentity>,
        intent: GoalUpdateIntent,
        ack: GoalUpdateAck,
    },
    OuterTurnFinished {
        previous_revision: u32,
        identity: Box<SurfaceGoalGenerationIdentity>,
        status: crate::runtime_surface::GoalOuterTurnStatus,
        usage: crate::runtime_surface::GoalUsage,
        next_action: crate::runtime_surface::GoalOuterTurnNextAction,
    },
    VerificationCompleted {
        previous_revision: u32,
        identity: Box<SurfaceGoalGenerationIdentity>,
        result: crate::runtime_surface::SurfaceGoalVerification,
    },
    Paused {
        previous_revision: u32,
        goal_run_id: GoalRunId,
        operation_id: SurfaceOperationId,
        outer_turn_id: Option<GoalOuterTurnId>,
        message: String,
    },
    PausedQuiescent {
        previous_revision: u32,
        message: String,
    },
    ContinuationStopped {
        previous_revision: u32,
        predecessor: Box<SurfaceGoalGenerationIdentity>,
        decision: Box<crate::runtime_surface::GoalContinuationDecision>,
    },
    ContinuationAdmitted {
        previous_revision: u32,
        predecessor: Box<SurfaceGoalGenerationIdentity>,
        decision: Box<crate::runtime_surface::GoalContinuationDecision>,
    },
    Recovered {
        previous_revision: u32,
        stale_goal_run_id: GoalRunId,
        operation: Box<OperationRecord>,
        origin: GoalTurnOrigin,
        stale_identity: Option<Box<SurfaceGoalGenerationIdentity>>,
        stale_run_settled: bool,
        recovery_message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum GoalSurfaceRowState {
    Present(GoalRecord),
    Removed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalSurfaceStoreReceipt {
    pub store_commit_id: String,
    pub goal_id: GoalId,
    pub goal_revision: u32,
    pub objective_revision: u32,
    pub catalog_revision: u32,
    pub goal_owner_epoch: u64,
    pub row_state: GoalSurfaceRowState,
    pub receipt_digest: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalSurfaceMutationRecord {
    pub session_id: String,
    pub mutation: GoalSurfaceMutation,
    pub receipt: GoalSurfaceStoreReceipt,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct LegacyGoalDb {
    goals: BTreeMap<String, ThreadGoal>,
}

struct StoredGoal {
    record: GoalRecord,
    created_at: i64,
    updated_at: i64,
}

struct StoredGoalSurfaceState {
    goal_id: GoalId,
    goal_revision: u32,
    objective_revision: u32,
    catalog_revision: u32,
    goal_owner_epoch: u64,
    row_present: bool,
    last_receipt_digest: [u8; 32],
}

struct StoredSurfaceMutation {
    session_id: String,
    store_commit_id: String,
    command_digest: Vec<u8>,
    receipt_digest: Vec<u8>,
    payload_json: String,
}

impl GoalStore {
    pub fn load_default() -> Result<Self, GoalStoreError> {
        let home = orca_home();
        Self::open_with_legacy(home.join(DATABASE_FILENAME), home.join(LEGACY_FILENAME))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, GoalStoreError> {
        Self::open_internal(path.as_ref().to_path_buf(), None)
    }

    pub fn open_with_legacy(
        path: impl AsRef<Path>,
        legacy_path: impl AsRef<Path>,
    ) -> Result<Self, GoalStoreError> {
        Self::open_internal(
            path.as_ref().to_path_buf(),
            Some(legacy_path.as_ref().to_path_buf()),
        )
    }

    fn open_internal(path: PathBuf, legacy_path: Option<PathBuf>) -> Result<Self, GoalStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let store = Self { path };
        store.initialize_schema_fenced()?;
        if let Some(legacy_path) = legacy_path.as_deref() {
            store.migrate_legacy_once(legacy_path)?;
        }
        Ok(store)
    }

    /// Initializes the schema under a sibling cross-process lock so concurrent
    /// openers of the same database serialize the DDL critical section instead
    /// of racing each other with `Immediate` transactions. DDL is idempotent
    /// (`IF NOT EXISTS`), so every opener passes through the same fenced path
    /// and the lock is held only for the short initialization.
    fn initialize_schema_fenced(&self) -> Result<(), GoalStoreError> {
        let lock_path = self.path.with_extension("sqlite3.lock");
        let _guard = ExclusiveFileLock::acquire(&lock_path)
            .map_err(|error| GoalStoreError::Io(io::Error::other(error)))?;
        self.initialize_schema()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        let version: String = connection.query_row(
            "SELECT value FROM goal_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )?;
        version.parse().map_err(|error| {
            GoalStoreError::Invalid(format!("invalid goal schema version '{version}': {error}"))
        })
    }

    pub fn current_surface_receipt_digest(
        &self,
        session_id: &str,
    ) -> Result<Option<[u8; 32]>, GoalStoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT last_receipt_digest
                 FROM goal_surface_state
                 WHERE session_id = ?1 AND row_present = 1",
                [session_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|digest| exact_digest(&digest, "receipt digest"))
            .transpose()
    }

    pub(crate) fn validate_surface_outer_turn_binding(
        &self,
        session_id: &str,
        identity: &SurfaceGoalGenerationIdentity,
    ) -> Result<GoalRecord, GoalStoreError> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn binding requires a session".to_string(),
            ));
        }
        let connection = self.connection()?;
        let stored = load_stored_goal(&connection, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal disappeared before outer-turn binding".to_string())
        })?;
        let run = stored.record.current_run.as_ref().ok_or_else(|| {
            GoalStoreError::Invalid("Goal outer-turn binding requires an open run".to_string())
        })?;
        if stored.record.goal_id.as_str() != identity.goal_id.as_str()
            || stored.record.objective_revision != identity.objective_revision.get()
            || run.goal_run_id.as_str() != identity.goal_run_id.as_str()
            || !run.in_flight
            || run.continuation_count != identity.outer_turn_count
            || run.outer_turn_id.as_ref().is_none_or(|outer_turn_id| {
                outer_turn_id.as_str() != identity.goal_outer_turn_id.as_str()
            })
        {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn binding identity is stale".to_string(),
            ));
        }
        let operation_json: String = connection.query_row(
            "SELECT surface_operation_json
             FROM goal_runs
             WHERE goal_run_id = ?1 AND goal_id = ?2 AND finished_at IS NULL",
            params![run.goal_run_id.as_str(), stored.record.goal_id.as_str()],
            |row| row.get(0),
        )?;
        let operation: OperationRecord = serde_json::from_str(&operation_json)?;
        validate_surface_goal_operation(
            &operation,
            &stored.record.goal_id,
            &run.goal_run_id,
            stored.record.objective_revision,
        )?;
        if operation.operation_id != identity.operation_fence.operation_id {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn binding operation identity is stale".to_string(),
            ));
        }
        Ok(stored.record)
    }

    pub(crate) fn claim_surface_owner_epoch(&self) -> Result<u64, GoalStoreError> {
        const OWNER_EPOCH_KEY: &str = "surface_owner_epoch";
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = transaction
            .query_row(
                "SELECT value FROM goal_meta WHERE key = ?1",
                [OWNER_EPOCH_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| {
                value.parse::<u64>().map_err(|_| {
                    GoalStoreError::Invalid(
                        "stored Goal surface owner epoch is not a u64".to_string(),
                    )
                })
            })
            .transpose()?
            .unwrap_or_default();
        let next = current.checked_add(1).ok_or_else(|| {
            GoalStoreError::Invalid("Goal surface owner epoch exhausted".to_string())
        })?;
        if next > i64::MAX as u64 {
            return Err(GoalStoreError::Invalid(
                "Goal surface owner epoch exceeds SQLite range".to_string(),
            ));
        }
        transaction.execute(
            "INSERT INTO goal_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![OWNER_EPOCH_KEY, next.to_string()],
        )?;
        transaction.commit()?;
        Ok(next)
    }

    pub fn create_goal(&self, input: CreateGoalInput) -> Result<GoalRecord, GoalStoreError> {
        validate_thread_goal_objective(&input.objective).map_err(GoalStoreError::Invalid)?;
        if input.session_id.trim().is_empty() {
            return Err(GoalStoreError::Invalid(
                "goal session id must not be empty".to_string(),
            ));
        }
        if input.token_budget.is_some_and(|budget| budget <= 0) {
            return Err(GoalStoreError::Invalid(
                "goal token budget must be positive".to_string(),
            ));
        }

        let goal_id = GoalId::new();
        let state = GoalState::Active;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_session_not_surface_owned(&transaction, input.session_id.trim(), "create")?;
        transaction.execute(
            "INSERT INTO goals (
                goal_id, session_id, objective, objective_revision, state,
                token_budget, created_at, updated_at
             ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?6)",
            params![
                goal_id.as_str(),
                input.session_id.trim(),
                input.objective.trim(),
                state_json(&state)?,
                input.token_budget,
                input.now,
            ],
        )?;
        insert_transition(
            &transaction,
            &goal_id,
            None,
            &state,
            &state,
            "created",
            input.now,
        )?;
        transaction.commit()?;
        Ok(self
            .get_by_session(input.session_id.trim())?
            .expect("created goal must be readable"))
    }

    pub(crate) fn create_goal_for_surface(
        &self,
        input: CreateGoalInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_thread_goal_objective(&input.objective).map_err(GoalStoreError::Invalid)?;
        if input.session_id.trim().is_empty() {
            return Err(GoalStoreError::Invalid(
                "goal session id must not be empty".to_string(),
            ));
        }
        if input.token_budget.is_some_and(|budget| budget <= 0) {
            return Err(GoalStoreError::Invalid(
                "goal token budget must be positive".to_string(),
            ));
        }
        validate_surface_mutation_context(&context)?;

        let session_id = input.session_id.trim().to_string();
        let objective = input.objective.trim().to_string();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        if transaction
            .query_row(
                "SELECT 1 FROM goals WHERE session_id = ?1",
                [&session_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(GoalStoreError::Invalid(
                "goal already exists for the surface session".to_string(),
            ));
        }
        let previous_catalog_revision = transaction
            .query_row(
                "SELECT catalog_revision FROM goal_surface_state WHERE session_id = ?1",
                [&session_id],
                |row| row.get::<_, u32>(0),
            )
            .optional()?
            .unwrap_or_default();
        let catalog_revision = previous_catalog_revision.checked_add(1).ok_or_else(|| {
            GoalStoreError::Invalid("goal catalog revision exhausted".to_string())
        })?;
        let goal_id = GoalId::new();
        let state = GoalState::Active;
        transaction.execute(
            "INSERT INTO goals (
                goal_id, session_id, objective, objective_revision, state,
                token_budget, created_at, updated_at
             ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?6)",
            params![
                goal_id.as_str(),
                session_id,
                objective,
                state_json(&state)?,
                input.token_budget,
                input.now,
            ],
        )?;
        insert_transition(
            &transaction,
            &goal_id,
            None,
            &state,
            &state,
            "created",
            input.now,
        )?;
        let record = load_stored_goal(&transaction, &session_id)?
            .expect("surface-created goal must be readable")
            .record;
        let mutation = GoalSurfaceMutation::Created;
        let row_state = GoalSurfaceRowState::Present(record);
        let receipt = goal_surface_receipt(
            &context,
            &session_id,
            &mutation,
            goal_id.clone(),
            1,
            1,
            catalog_revision,
            row_state,
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.clone(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, input.now)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub(crate) fn create_goal_and_prepare_run_for_surface(
        &self,
        input: CreateGoalAndPrepareRunForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_thread_goal_objective(&input.goal.objective).map_err(GoalStoreError::Invalid)?;
        if input.goal.session_id.trim().is_empty() {
            return Err(GoalStoreError::Invalid(
                "goal session id must not be empty".to_string(),
            ));
        }
        if input.goal.token_budget.is_some_and(|budget| budget <= 0) {
            return Err(GoalStoreError::Invalid(
                "goal token budget must be positive".to_string(),
            ));
        }
        if input.origin == GoalTurnOrigin::Continuation {
            return Err(GoalStoreError::Invalid(
                "a continuation cannot prepare a new Goal run".to_string(),
            ));
        }
        validate_surface_mutation_context(&context)?;
        validate_surface_goal_operation(&input.operation, &input.goal_id, &input.goal_run_id, 1)?;
        let session_id = input.goal.session_id.trim().to_string();
        let objective = input.goal.objective.trim().to_string();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        if transaction
            .query_row(
                "SELECT 1 FROM goals WHERE session_id = ?1",
                [&session_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(GoalStoreError::Invalid(
                "goal already exists for the surface session".to_string(),
            ));
        }
        let previous_catalog_revision = transaction
            .query_row(
                "SELECT catalog_revision FROM goal_surface_state WHERE session_id = ?1",
                [&session_id],
                |row| row.get::<_, u32>(0),
            )
            .optional()?
            .unwrap_or_default();
        let catalog_revision = previous_catalog_revision.checked_add(1).ok_or_else(|| {
            GoalStoreError::Invalid("goal catalog revision exhausted".to_string())
        })?;
        let goal_id = input.goal_id;
        let state = GoalState::Active;
        transaction.execute(
            "INSERT INTO goals (
                goal_id, session_id, objective, objective_revision, state,
                token_budget, created_at, updated_at
             ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?6)",
            params![
                goal_id.as_str(),
                session_id,
                objective,
                state_json(&state)?,
                input.goal.token_budget,
                input.goal.now,
            ],
        )?;
        insert_transition(
            &transaction,
            &goal_id,
            None,
            &state,
            &state,
            "created",
            input.goal.now,
        )?;
        transaction.execute(
            "INSERT INTO goal_runs (
                goal_run_id, goal_id, status, origin, current_outer_turn_id,
                continuation_count, in_flight, started_at, finished_at,
                surface_operation_id, surface_operation_json
             ) VALUES (?1, ?2, 'preparing', ?3, NULL, 0, 0, ?4, NULL, ?5, ?6)",
            params![
                input.goal_run_id.as_str(),
                goal_id.as_str(),
                origin_name(input.origin),
                input.goal.now,
                uuid::Uuid::from_bytes(*input.operation.operation_id.as_bytes())
                    .hyphenated()
                    .to_string(),
                serde_json::to_string(&input.operation)?,
            ],
        )?;
        let record = load_stored_goal(&transaction, &session_id)?
            .expect("surface-created Goal with preparing run must be readable")
            .record;
        let mutation = GoalSurfaceMutation::CreatedWithRun {
            goal_run_id: input.goal_run_id,
            operation: input.operation,
            origin: input.origin,
        };
        let receipt = goal_surface_receipt(
            &context,
            &session_id,
            &mutation,
            goal_id,
            1,
            1,
            catalog_revision,
            GoalSurfaceRowState::Present(record),
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.clone(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, input.goal.now)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub(crate) fn adopt_goal_for_surface(
        &self,
        session_id: &str,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_surface_mutation_context(&context)?;
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err(GoalStoreError::Invalid(
                "goal session id must not be empty".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        let previous_state = load_goal_surface_state(&transaction, session_id)?;
        if previous_state
            .as_ref()
            .is_some_and(|state| state.row_present)
        {
            return Err(GoalStoreError::Invalid(
                "goal already has a durable surface owner".to_string(),
            ));
        }
        let mut stored = load_stored_goal(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("legacy goal does not exist for adoption".to_string())
        })?;
        ensure_goal_not_in_flight(&transaction, stored.record.goal_id.as_str(), "adopt")?;
        if stored.record.current_run.is_some() {
            let adopted_at = Utc::now().timestamp();
            let previous = stored.record.state.clone();
            let next = if matches!(previous, GoalState::Complete { .. }) {
                previous.clone()
            } else {
                GoalState::Paused {
                    reason: GoalPauseReason::Recovery,
                    message: "closed a legacy Goal run without typed operation ownership"
                        .to_string(),
                }
            };
            transaction.execute(
                "UPDATE goal_runs
                 SET status = 'recovered', in_flight = 0,
                     finished_at = COALESCE(finished_at, ?1)
                 WHERE goal_id = ?2 AND finished_at IS NULL",
                params![adopted_at, stored.record.goal_id.as_str()],
            )?;
            if previous != next {
                transaction.execute(
                    "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                    params![
                        state_json(&next)?,
                        adopted_at,
                        stored.record.goal_id.as_str()
                    ],
                )?;
                insert_transition(
                    &transaction,
                    &stored.record.goal_id,
                    None,
                    &previous,
                    &next,
                    "surface_adoption_recovery",
                    adopted_at,
                )?;
            }
            stored = load_stored_goal(&transaction, session_id)?
                .expect("adopted legacy Goal remains readable");
        }
        let catalog_revision = previous_state.map_or(Ok(1), |state| {
            state.catalog_revision.checked_add(1).ok_or_else(|| {
                GoalStoreError::Invalid("goal catalog revision exhausted".to_string())
            })
        })?;
        let mutation = GoalSurfaceMutation::Created;
        let receipt = goal_surface_receipt(
            &context,
            session_id,
            &mutation,
            stored.record.goal_id.clone(),
            1,
            stored.record.objective_revision,
            catalog_revision,
            GoalSurfaceRowState::Present(stored.record),
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, stored.updated_at)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub(crate) fn clear_goal_for_surface(
        &self,
        session_id: &str,
        expected_goal_id: &GoalId,
        expected_goal_revision: u32,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_surface_mutation_context(&context)?;
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err(GoalStoreError::Invalid(
                "goal session id must not be empty".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        if !state.row_present
            || &state.goal_id != expected_goal_id
            || state.goal_revision != expected_goal_revision
        {
            return Err(GoalStoreError::Invalid(
                "goal surface fence is stale".to_string(),
            ));
        }
        ensure_goal_not_in_flight(&transaction, expected_goal_id.as_str(), "clear")?;
        let changed =
            transaction.execute("DELETE FROM goals WHERE session_id = ?1", [session_id])?;
        if changed != 1 {
            return Err(GoalStoreError::Invalid(
                "goal disappeared before surface clear".to_string(),
            ));
        }
        let tombstone_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let catalog_revision = state.catalog_revision.checked_add(1).ok_or_else(|| {
            GoalStoreError::Invalid("goal catalog revision exhausted".to_string())
        })?;
        let mutation = GoalSurfaceMutation::Removed {
            previous_revision: state.goal_revision,
            tombstone_revision,
        };
        let retained_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..context.clone()
        };
        let receipt = goal_surface_receipt(
            &retained_context,
            session_id,
            &mutation,
            state.goal_id,
            tombstone_revision,
            state.objective_revision,
            catalog_revision,
            GoalSurfaceRowState::Removed,
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, Utc::now().timestamp())?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn edit_goal_for_surface(
        &self,
        session_id: &str,
        expected_goal_id: &GoalId,
        expected_goal_revision: u32,
        objective: &str,
        token_budget_update: GoalSurfaceTokenBudgetUpdate,
        updated_at: i64,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_thread_goal_objective(objective).map_err(GoalStoreError::Invalid)?;
        if matches!(
            token_budget_update,
            GoalSurfaceTokenBudgetUpdate::Set(Some(budget)) if budget <= 0
        ) {
            return Err(GoalStoreError::Invalid(
                "goal token budget must be positive".to_string(),
            ));
        }
        validate_surface_mutation_context(&context)?;
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err(GoalStoreError::Invalid(
                "goal session id must not be empty".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        if !state.row_present
            || &state.goal_id != expected_goal_id
            || state.goal_revision != expected_goal_revision
        {
            return Err(GoalStoreError::Invalid(
                "goal surface fence is stale".to_string(),
            ));
        }
        let stored = load_stored_goal(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal disappeared before surface edit".to_string())
        })?;
        ensure_goal_not_in_flight(&transaction, expected_goal_id.as_str(), "edit")?;
        let token_budget = match token_budget_update {
            GoalSurfaceTokenBudgetUpdate::Keep => stored.record.token_budget,
            GoalSurfaceTokenBudgetUpdate::Set(token_budget) => token_budget,
        };
        let objective = objective.trim();
        let objective_revision_increment =
            i64::from(u8::from(stored.record.objective != objective));
        let previous_state = stored.record.state;
        let next_state = GoalState::Active;
        transaction.execute(
            "UPDATE goals SET objective = ?1,
                objective_revision = objective_revision + ?2,
                state = ?3, token_budget = ?4, updated_at = ?5 WHERE session_id = ?6",
            params![
                objective,
                objective_revision_increment,
                state_json(&next_state)?,
                token_budget,
                updated_at,
                session_id
            ],
        )?;
        transaction.execute(
            "UPDATE goal_runs
             SET status = 'edited', in_flight = 0, finished_at = COALESCE(finished_at, ?1)
             WHERE goal_id = ?2 AND finished_at IS NULL",
            params![updated_at, expected_goal_id.as_str()],
        )?;
        insert_transition(
            &transaction,
            expected_goal_id,
            None,
            &previous_state,
            &next_state,
            "edited",
            updated_at,
        )?;
        let record = load_stored_goal(&transaction, session_id)?
            .expect("surface-edited goal must be readable")
            .record;
        let goal_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let mutation = GoalSurfaceMutation::Edited {
            previous_revision: state.goal_revision,
        };
        let retained_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..context.clone()
        };
        let receipt = goal_surface_receipt(
            &retained_context,
            session_id,
            &mutation,
            state.goal_id,
            goal_revision,
            record.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(record),
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, updated_at)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub(crate) fn prepare_goal_run_for_surface(
        &self,
        input: PrepareGoalRunForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_surface_mutation_context(&context)?;
        let session_id = input.session_id.trim();
        if session_id.is_empty() {
            return Err(GoalStoreError::Invalid(
                "goal session id must not be empty".to_string(),
            ));
        }
        if input.origin == GoalTurnOrigin::Continuation {
            return Err(GoalStoreError::Invalid(
                "a continuation cannot prepare a new Goal run".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        if !state.row_present
            || state.goal_id != input.expected_goal_id
            || state.goal_revision != input.expected_goal_revision
            || input.expected_receipt_digest != state.last_receipt_digest
        {
            return Err(GoalStoreError::Invalid(
                "goal surface fence is stale".to_string(),
            ));
        }
        validate_surface_goal_operation(
            &input.operation,
            &input.expected_goal_id,
            &input.goal_run_id,
            state.objective_revision,
        )?;
        let stored = load_stored_goal(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal disappeared before surface run preparation".to_string())
        })?;
        if input.origin == GoalTurnOrigin::Resume {
            if matches!(stored.record.state, GoalState::Complete { .. }) {
                return Err(GoalStoreError::Invalid(
                    "cannot resume a completed Goal".to_string(),
                ));
            }
            if stored.record.state != GoalState::Active {
                transaction.execute(
                    "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                    params![
                        state_json(&GoalState::Active)?,
                        input.started_at,
                        input.expected_goal_id.as_str()
                    ],
                )?;
                insert_transition(
                    &transaction,
                    &input.expected_goal_id,
                    None,
                    &stored.record.state,
                    &GoalState::Active,
                    "surface_resumed",
                    input.started_at,
                )?;
            }
        } else if !stored.record.state.should_continue() {
            return Err(GoalStoreError::Invalid(format!(
                "cannot prepare a Goal run while state is {:?}",
                stored.record.state
            )));
        }
        if stored.record.current_run.is_some() {
            return Err(GoalStoreError::Invalid(
                "cannot prepare a Goal run while another run is open".to_string(),
            ));
        }
        let operation_id = uuid::Uuid::from_bytes(*input.operation.operation_id.as_bytes())
            .hyphenated()
            .to_string();
        transaction.execute(
            "INSERT INTO goal_runs (
                goal_run_id, goal_id, status, origin, current_outer_turn_id,
                continuation_count, in_flight, started_at, finished_at,
                surface_operation_id, surface_operation_json
             ) VALUES (?1, ?2, 'preparing', ?3, NULL, 0, 0, ?4, NULL, ?5, ?6)",
            params![
                input.goal_run_id.as_str(),
                input.expected_goal_id.as_str(),
                origin_name(input.origin),
                input.started_at,
                operation_id,
                serde_json::to_string(&input.operation)?,
            ],
        )?;
        let record = load_stored_goal(&transaction, session_id)?
            .expect("surface-prepared Goal run must be readable")
            .record;
        let goal_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let mutation = GoalSurfaceMutation::RunStarted {
            previous_revision: state.goal_revision,
            goal_run_id: input.goal_run_id,
            operation: input.operation,
            origin: input.origin,
        };
        let retained_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..context.clone()
        };
        let receipt = goal_surface_receipt(
            &retained_context,
            session_id,
            &mutation,
            state.goal_id,
            goal_revision,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(record),
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, input.started_at)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub(crate) fn edit_goal_and_prepare_run_for_surface(
        &self,
        input: EditGoalAndPrepareRunForSurfaceInput,
        contexts: [GoalSurfaceMutationContext; 2],
    ) -> Result<Vec<GoalSurfaceMutationRecord>, GoalStoreError> {
        let [edit_context, run_context] = contexts;
        validate_thread_goal_objective(&input.objective).map_err(GoalStoreError::Invalid)?;
        if input.token_budget.is_some_and(|budget| budget <= 0)
            || input.origin == GoalTurnOrigin::Continuation
            || edit_context.store_commit_id == run_context.store_commit_id
            || edit_context.goal_owner_epoch != run_context.goal_owner_epoch
        {
            return Err(GoalStoreError::Invalid(
                "Goal edit-and-run input is invalid".to_string(),
            ));
        }
        validate_surface_mutation_context(&edit_context)?;
        validate_surface_mutation_context(&run_context)?;
        let session_id = input.session_id.trim();
        if session_id.is_empty() {
            return Err(GoalStoreError::Invalid(
                "goal session id must not be empty".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, edit_context.goal_owner_epoch)?;
        let replay_edit = replay_surface_mutation(&transaction, &edit_context)?;
        let replay_run = replay_surface_mutation(&transaction, &run_context)?;
        match (replay_edit, replay_run) {
            (Some(edit), Some(run)) => {
                transaction.commit()?;
                return Ok(vec![edit, run]);
            }
            (None, None) => {}
            _ => {
                return Err(GoalStoreError::Invalid(
                    "Goal edit-and-run replay is incomplete".to_string(),
                ));
            }
        }
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        if !state.row_present
            || state.goal_id != input.expected_goal_id
            || state.goal_revision != input.expected_goal_revision
            || input.expected_receipt_digest != state.last_receipt_digest
        {
            return Err(GoalStoreError::Invalid(
                "Goal edit-and-run fence is stale".to_string(),
            ));
        }
        let stored = load_stored_goal(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal disappeared before edit-and-run".to_string())
        })?;
        if stored.record.current_run.is_some()
            || matches!(stored.record.state, GoalState::Complete { .. })
        {
            return Err(GoalStoreError::Invalid(
                "Goal edit-and-run requires an inactive Goal without an open run".to_string(),
            ));
        }
        let objective = input.objective.trim();
        let objective_revision_increment =
            i64::from(u8::from(stored.record.objective != objective));
        transaction.execute(
            "UPDATE goals SET objective = ?1,
                objective_revision = objective_revision + ?2,
                state = ?3, token_budget = ?4, updated_at = ?5 WHERE session_id = ?6",
            params![
                objective,
                objective_revision_increment,
                state_json(&GoalState::Active)?,
                input.token_budget,
                input.started_at,
                session_id
            ],
        )?;
        if stored.record.state != GoalState::Active {
            insert_transition(
                &transaction,
                &input.expected_goal_id,
                None,
                &stored.record.state,
                &GoalState::Active,
                "surface_set_and_run",
                input.started_at,
            )?;
        }
        let edited_record = load_stored_goal(&transaction, session_id)?
            .expect("edited Goal remains readable")
            .record;
        let edited_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let edited_mutation = GoalSurfaceMutation::Edited {
            previous_revision: state.goal_revision,
        };
        let retained_edit_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..edit_context.clone()
        };
        let edited_receipt = goal_surface_receipt(
            &retained_edit_context,
            session_id,
            &edited_mutation,
            state.goal_id.clone(),
            edited_revision,
            edited_record.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(edited_record.clone()),
        )?;
        let edited = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation: edited_mutation,
            receipt: edited_receipt,
        };
        persist_surface_mutation(&transaction, &edit_context, &edited, input.started_at)?;
        persist_surface_state(&transaction, &edited)?;

        validate_surface_goal_operation(
            &input.operation,
            &input.expected_goal_id,
            &input.goal_run_id,
            edited_record.objective_revision,
        )?;
        let operation_id = uuid::Uuid::from_bytes(*input.operation.operation_id.as_bytes())
            .hyphenated()
            .to_string();
        transaction.execute(
            "INSERT INTO goal_runs (
                goal_run_id, goal_id, status, origin, current_outer_turn_id,
                continuation_count, in_flight, started_at, finished_at,
                surface_operation_id, surface_operation_json
             ) VALUES (?1, ?2, 'preparing', ?3, NULL, 0, 0, ?4, NULL, ?5, ?6)",
            params![
                input.goal_run_id.as_str(),
                input.expected_goal_id.as_str(),
                origin_name(input.origin),
                input.started_at,
                operation_id,
                serde_json::to_string(&input.operation)?,
            ],
        )?;
        let run_record = load_stored_goal(&transaction, session_id)?
            .expect("Goal run remains readable")
            .record;
        let run_revision = edited_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let run_mutation = GoalSurfaceMutation::RunStarted {
            previous_revision: edited_revision,
            goal_run_id: input.goal_run_id,
            operation: input.operation,
            origin: input.origin,
        };
        let retained_run_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..run_context.clone()
        };
        let run_receipt = goal_surface_receipt(
            &retained_run_context,
            session_id,
            &run_mutation,
            state.goal_id,
            run_revision,
            run_record.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(run_record),
        )?;
        let run = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation: run_mutation,
            receipt: run_receipt,
        };
        persist_surface_mutation(&transaction, &run_context, &run, input.started_at)?;
        persist_surface_state(&transaction, &run)?;
        transaction.commit()?;
        Ok(vec![edited, run])
    }

    pub(crate) fn begin_goal_outer_turn_for_surface(
        &self,
        input: BeginGoalOuterTurnForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_surface_mutation_context(&context)?;
        let session_id = input.session_id.trim();
        if session_id.is_empty() || input.provider_turn_id.trim().is_empty() {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn admission requires a session and provider turn".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        if !state.row_present
            || state.goal_id != input.expected_goal_id
            || state.goal_revision != input.expected_goal_revision
        {
            return Err(GoalStoreError::Invalid(
                "goal surface fence is stale".to_string(),
            ));
        }
        let identity = input.identity.as_ref();
        if identity.goal_id.as_str() != input.expected_goal_id.as_str()
            || identity.objective_revision.get() != state.objective_revision
            || identity.operation_fence.generation_id.get() != 0
            || identity.predecessor_fence.is_some()
            || identity.outer_turn_count != 1
            || identity.attempt != GenerationAttempt::Initial
        {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn identity does not match the initial run fence".to_string(),
            ));
        }
        let stored = load_stored_goal(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal disappeared before outer-turn admission".to_string())
        })?;
        let current_run = stored.record.current_run.as_ref().ok_or_else(|| {
            GoalStoreError::Invalid(
                "Goal outer-turn admission requires a preparing run".to_string(),
            )
        })?;
        if current_run.goal_run_id.as_str() != identity.goal_run_id.as_str()
            || current_run.in_flight
            || current_run.outer_turn_id.is_some()
            || current_run.continuation_count != 0
            || current_run.origin == GoalTurnOrigin::Continuation
        {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn admission does not match the preparing run".to_string(),
            ));
        }
        let operation_json: String = transaction.query_row(
            "SELECT surface_operation_json
             FROM goal_runs
             WHERE goal_run_id = ?1 AND goal_id = ?2 AND finished_at IS NULL",
            params![
                current_run.goal_run_id.as_str(),
                input.expected_goal_id.as_str()
            ],
            |row| row.get(0),
        )?;
        let operation: OperationRecord = serde_json::from_str(&operation_json)?;
        validate_surface_goal_operation(
            &operation,
            &input.expected_goal_id,
            &current_run.goal_run_id,
            state.objective_revision,
        )?;
        if operation.operation_id != identity.operation_fence.operation_id {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn operation fence is not the durable run binding".to_string(),
            ));
        }
        let outer_turn_id =
            GoalOuterTurnId::parse(identity.goal_outer_turn_id.as_str().to_string())
                .map_err(GoalStoreError::Invalid)?;
        let changed = transaction.execute(
            "UPDATE goal_runs
             SET status = 'active',
                 current_outer_turn_id = ?1,
                 continuation_count = 1,
                 in_flight = 1
             WHERE goal_run_id = ?2 AND goal_id = ?3
               AND in_flight = 0 AND current_outer_turn_id IS NULL
               AND continuation_count = 0 AND finished_at IS NULL",
            params![
                outer_turn_id.as_str(),
                current_run.goal_run_id.as_str(),
                input.expected_goal_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(GoalStoreError::Invalid(
                "Goal preparing run changed before outer-turn admission".to_string(),
            ));
        }
        transaction.execute(
            "INSERT INTO goal_turns (
                outer_turn_id, goal_run_id, origin, provider_turn_id, status,
                tool_count, model_response_count, charged_input_tokens,
                output_tokens, verifier_tokens, gap_fingerprint, started_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, 'in_flight', 0, 0, 0, 0, 0, NULL, ?5, NULL)",
            params![
                outer_turn_id.as_str(),
                current_run.goal_run_id.as_str(),
                origin_name(current_run.origin),
                input.provider_turn_id.trim(),
                input.started_at,
            ],
        )?;
        let record = load_stored_goal(&transaction, session_id)?
            .expect("surface-started Goal outer turn remains readable")
            .record;
        let goal_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let mutation = GoalSurfaceMutation::OuterTurnStarted {
            previous_revision: state.goal_revision,
            identity: input.identity,
        };
        let retained_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..context.clone()
        };
        let receipt = goal_surface_receipt(
            &retained_context,
            session_id,
            &mutation,
            state.goal_id,
            goal_revision,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(record),
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, input.started_at)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub(crate) fn finish_goal_outer_turn_for_surface(
        &self,
        input: FinishGoalOuterTurnForSurfaceInput,
        contexts: Vec<GoalSurfaceMutationContext>,
    ) -> Result<Vec<GoalSurfaceMutationRecord>, GoalStoreError> {
        let expected_contexts = if input.verification.is_some() { 3 } else { 2 };
        if contexts.len() != expected_contexts {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn settlement context count is invalid".to_string(),
            ));
        }
        let finish_context = contexts[0].clone();
        let verification_context = input.verification.as_ref().map(|_| contexts[1].clone());
        let decision_context = contexts[expected_contexts - 1].clone();
        validate_surface_mutation_context(&finish_context)?;
        validate_surface_mutation_context(&decision_context)?;
        if let Some(context) = verification_context.as_ref() {
            validate_surface_mutation_context(context)?;
        }
        if finish_context.store_commit_id == decision_context.store_commit_id
            || finish_context.goal_owner_epoch != decision_context.goal_owner_epoch
            || verification_context.as_ref().is_some_and(|context| {
                context.store_commit_id == finish_context.store_commit_id
                    || context.store_commit_id == decision_context.store_commit_id
                    || context.goal_owner_epoch != finish_context.goal_owner_epoch
            })
            || input.session_id.trim().is_empty()
            || input.pause_message.trim().is_empty()
        {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn settlement contexts are invalid".to_string(),
            ));
        }
        let session_id = input.session_id.trim();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, finish_context.goal_owner_epoch)?;
        let replay_finish = replay_surface_mutation(&transaction, &finish_context)?;
        let replay_verification = verification_context
            .as_ref()
            .map(|context| replay_surface_mutation(&transaction, context))
            .transpose()?;
        let replay_decision = replay_surface_mutation(&transaction, &decision_context)?;
        if replay_finish.is_some()
            && replay_decision.is_some()
            && replay_verification
                .as_ref()
                .is_none_or(|verification| verification.is_some())
        {
            let mut replayed = vec![replay_finish.expect("checked finish")];
            if let Some(verification) = replay_verification.flatten() {
                replayed.push(verification);
            }
            replayed.push(replay_decision.expect("checked decision"));
            transaction.commit()?;
            return Ok(replayed);
        }
        if replay_finish.is_some()
            || replay_decision.is_some()
            || replay_verification
                .as_ref()
                .is_some_and(|verification| verification.is_some())
        {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn settlement replay is incomplete".to_string(),
            ));
        }
        let continuation_predecessor_is_resumable = matches!(
            (&input.status, &input.terminal),
            (
                crate::runtime_surface::GoalOuterTurnStatus::Success,
                crate::runtime_surface::OperationTerminal::Succeeded { .. }
            ) | (
                crate::runtime_surface::GoalOuterTurnStatus::BudgetExhausted,
                crate::runtime_surface::OperationTerminal::BudgetExhausted {
                    budget: crate::runtime_surface::OperationBudget::TurnRequests {
                        scope: crate::runtime_surface::TurnRequestBudgetScope::AgentLoop,
                        ..
                    },
                }
            )
        );
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        if !state.row_present
            || state.goal_id != input.expected_goal_id
            || state.goal_revision != input.expected_goal_revision
            || input.identity.goal_id.as_str() != input.expected_goal_id.as_str()
            || input.identity.objective_revision.get() != state.objective_revision
            || input.continuation.as_ref().is_some_and(|continuation| {
                !continuation_predecessor_is_resumable
                    || input.next_action
                        != crate::runtime_surface::GoalOuterTurnNextAction::Continue
                    || continuation.provider_turn_id.trim().is_empty()
                    || continuation.successor.goal_id != input.identity.goal_id
                    || continuation.successor.goal_run_id != input.identity.goal_run_id
                    || continuation.successor.operation_fence.operation_id
                        != input.identity.operation_fence.operation_id
                    || continuation.successor.operation_fence.thread_id
                        != input.identity.operation_fence.thread_id
                    || continuation.successor.operation_fence.thread_owner_epoch
                        != input.identity.operation_fence.thread_owner_epoch
                    || input
                        .identity
                        .operation_fence
                        .generation_id
                        .get()
                        .checked_add(1)
                        != Some(continuation.successor.operation_fence.generation_id.get())
                    || continuation.successor.predecessor_fence.as_ref()
                        != Some(&input.identity.operation_fence)
                    || input.identity.outer_turn_count.checked_add(1)
                        != Some(continuation.successor.outer_turn_count)
                    || continuation.successor.objective_revision
                        != input.identity.objective_revision
                    || continuation.successor.outer_turn_origin
                        != crate::runtime_surface::GoalOuterTurnOrigin::Continuation
            })
        {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn settlement fence is stale".to_string(),
            ));
        }
        let current = load_stored_goal(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal disappeared before outer-turn settlement".to_string())
        })?;
        let current_run = current.record.current_run.as_ref().ok_or_else(|| {
            GoalStoreError::Invalid("Goal outer-turn settlement requires a live run".to_string())
        })?;
        let operation_json: String = transaction.query_row(
            "SELECT surface_operation_json
             FROM goal_runs
             WHERE goal_run_id = ?1 AND finished_at IS NULL",
            [current_run.goal_run_id.as_str()],
            |row| row.get(0),
        )?;
        let operation: OperationRecord = serde_json::from_str(&operation_json)?;
        if current_run.goal_run_id.as_str() != input.identity.goal_run_id.as_str()
            || !current_run.in_flight
            || current_run.continuation_count != input.identity.outer_turn_count
            || operation.operation_id != input.identity.operation_fence.operation_id
            || current_run
                .outer_turn_id
                .as_ref()
                .is_none_or(|outer_turn_id| {
                    outer_turn_id.as_str() != input.identity.goal_outer_turn_id.as_str()
                })
        {
            return Err(GoalStoreError::Invalid(
                "Goal outer-turn settlement identity is stale".to_string(),
            ));
        }
        let core_status = match input.status {
            crate::runtime_surface::GoalOuterTurnStatus::Success => GoalTurnStatus::Success,
            crate::runtime_surface::GoalOuterTurnStatus::Failed => GoalTurnStatus::Failed,
            crate::runtime_surface::GoalOuterTurnStatus::Cancelled => GoalTurnStatus::Cancelled,
            crate::runtime_surface::GoalOuterTurnStatus::ApprovalRequired => {
                GoalTurnStatus::ApprovalRequired
            }
            crate::runtime_surface::GoalOuterTurnStatus::BudgetExhausted => {
                GoalTurnStatus::BudgetExhausted
            }
        };
        let core_usage = GoalUsage {
            charged_input_tokens: input.usage.charged_input_tokens,
            output_tokens: input.usage.output_tokens,
            cache_tokens: input.usage.cache_tokens,
            verifier_tokens: input.usage.verifier_tokens,
            cost_micros: input.usage.cost_micros,
            elapsed_seconds: input.usage.elapsed_seconds,
        };
        insert_usage_event(
            &transaction,
            &GoalUsageEvent {
                usage_event_id: format!(
                    "surface-generation:{}:{}",
                    uuid::Uuid::from_bytes(*input.identity.operation_fence.operation_id.as_bytes()),
                    input.identity.operation_fence.generation_id.get()
                ),
                goal_id: input.expected_goal_id.clone(),
                source: "surface-generation".to_string(),
                usage: core_usage.clone(),
                created_at: input.finished_at,
            },
        )?;
        let changed = transaction.execute(
            "UPDATE goal_turns
             SET status = ?1, tool_count = ?2, model_response_count = ?3,
                 charged_input_tokens = ?4, output_tokens = ?5,
                 verifier_tokens = ?6, gap_fingerprint = ?7, finished_at = ?8
             WHERE outer_turn_id = ?9 AND goal_run_id = ?10 AND status = 'in_flight'",
            params![
                turn_status_name(core_status),
                input.progress.tool_count,
                input.progress.model_response_count,
                core_usage.charged_input_tokens.max(0),
                core_usage.output_tokens.max(0),
                core_usage.verifier_tokens.max(0),
                input.progress.gap_fingerprint.as_deref(),
                input.finished_at,
                input.identity.goal_outer_turn_id.as_str(),
                current_run.goal_run_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(GoalStoreError::Invalid(
                "Goal outer turn was concurrently settled".to_string(),
            ));
        }
        transaction.execute(
            "UPDATE goal_runs SET current_outer_turn_id = NULL, in_flight = 0
             WHERE goal_run_id = ?1 AND goal_id = ?2 AND in_flight = 1",
            params![
                current_run.goal_run_id.as_str(),
                input.expected_goal_id.as_str()
            ],
        )?;
        let settled_record = load_stored_goal(&transaction, session_id)?
            .expect("settled surface Goal remains readable")
            .record;
        let finished_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let finished_mutation = GoalSurfaceMutation::OuterTurnFinished {
            previous_revision: state.goal_revision,
            identity: input.identity.clone(),
            status: input.status,
            usage: input.usage.clone(),
            next_action: input.next_action,
        };
        let retained_finish_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..finish_context.clone()
        };
        let finished_receipt = goal_surface_receipt(
            &retained_finish_context,
            session_id,
            &finished_mutation,
            state.goal_id.clone(),
            finished_revision,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(settled_record.clone()),
        )?;
        let finished_record = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation: finished_mutation,
            receipt: finished_receipt,
        };
        persist_surface_mutation(
            &transaction,
            &finish_context,
            &finished_record,
            input.finished_at,
        )?;
        persist_surface_state(&transaction, &finished_record)?;
        let mut outputs = vec![finished_record];
        let mut decision_previous_revision = finished_revision;
        if let (Some(result), Some(context)) =
            (input.verification.clone(), verification_context.as_ref())
        {
            let verification_revision = finished_revision
                .checked_add(1)
                .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
            let verification_mutation = GoalSurfaceMutation::VerificationCompleted {
                previous_revision: finished_revision,
                identity: input.identity.clone(),
                result,
            };
            let verification_receipt = goal_surface_receipt(
                &GoalSurfaceMutationContext {
                    goal_owner_epoch: state.goal_owner_epoch,
                    ..context.clone()
                },
                session_id,
                &verification_mutation,
                state.goal_id.clone(),
                verification_revision,
                state.objective_revision,
                state.catalog_revision,
                GoalSurfaceRowState::Present(settled_record),
            )?;
            let verification_record = GoalSurfaceMutationRecord {
                session_id: session_id.to_string(),
                mutation: verification_mutation,
                receipt: verification_receipt,
            };
            persist_surface_mutation(
                &transaction,
                context,
                &verification_record,
                input.finished_at,
            )?;
            persist_surface_state(&transaction, &verification_record)?;
            decision_previous_revision = verification_revision;
            outputs.push(verification_record);
        }

        let previous_state = current.record.state;
        if let Some(continuation) = input.continuation {
            let successor_outer_turn_id = GoalOuterTurnId::parse(
                continuation
                    .successor
                    .goal_outer_turn_id
                    .as_str()
                    .to_string(),
            )
            .map_err(GoalStoreError::Invalid)?;
            let changed = transaction.execute(
                "UPDATE goal_runs
                 SET status = 'active', current_outer_turn_id = ?1,
                     continuation_count = ?2, in_flight = 1
                 WHERE goal_run_id = ?3 AND goal_id = ?4
                   AND in_flight = 0 AND current_outer_turn_id IS NULL
                   AND continuation_count = ?5 AND finished_at IS NULL",
                params![
                    successor_outer_turn_id.as_str(),
                    continuation.successor.outer_turn_count,
                    current_run.goal_run_id.as_str(),
                    input.expected_goal_id.as_str(),
                    input.identity.outer_turn_count,
                ],
            )?;
            if changed != 1 {
                return Err(GoalStoreError::Invalid(
                    "Goal continuation successor lost its durable predecessor fence".to_string(),
                ));
            }
            transaction.execute(
                "INSERT INTO goal_turns (
                    outer_turn_id, goal_run_id, origin, provider_turn_id, status,
                    tool_count, model_response_count, charged_input_tokens,
                    output_tokens, verifier_tokens, gap_fingerprint, started_at, finished_at
                 ) VALUES (?1, ?2, ?3, ?4, 'in_flight', 0, 0, 0, 0, 0, NULL, ?5, NULL)",
                params![
                    successor_outer_turn_id.as_str(),
                    current_run.goal_run_id.as_str(),
                    origin_name(GoalTurnOrigin::Continuation),
                    continuation.provider_turn_id.trim(),
                    input.finished_at,
                ],
            )?;
            let decision = crate::runtime_surface::GoalContinuationDecision::Admitted {
                reason: continuation.reason,
                successor: continuation.successor.as_ref().clone(),
            };
            let decision_record = load_stored_goal(&transaction, session_id)?
                .expect("admitted surface Goal continuation remains readable")
                .record;
            let decision_revision = decision_previous_revision
                .checked_add(1)
                .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
            let decision_mutation = GoalSurfaceMutation::ContinuationAdmitted {
                previous_revision: decision_previous_revision,
                predecessor: input.identity,
                decision: Box::new(decision),
            };
            let retained_decision_context = GoalSurfaceMutationContext {
                goal_owner_epoch: state.goal_owner_epoch,
                ..decision_context.clone()
            };
            let decision_receipt = goal_surface_receipt(
                &retained_decision_context,
                session_id,
                &decision_mutation,
                state.goal_id,
                decision_revision,
                state.objective_revision,
                state.catalog_revision,
                GoalSurfaceRowState::Present(decision_record),
            )?;
            let decision_record = GoalSurfaceMutationRecord {
                session_id: session_id.to_string(),
                mutation: decision_mutation,
                receipt: decision_receipt,
            };
            persist_surface_mutation(
                &transaction,
                &decision_context,
                &decision_record,
                input.finished_at,
            )?;
            persist_surface_state(&transaction, &decision_record)?;
            transaction.commit()?;
            outputs.push(decision_record);
            return Ok(outputs);
        }
        let pause_reason = match &input.stop_reason {
            crate::runtime_surface::GoalContinuationStopReason::TerminalizingControl {
                cause:
                    crate::runtime_surface::TerminalizationCause::UserCancel
                    | crate::runtime_surface::TerminalizationCause::GoalPause,
            }
            | crate::runtime_surface::GoalContinuationStopReason::QueuedUserInput { .. } => {
                GoalPauseReason::User
            }
            crate::runtime_surface::GoalContinuationStopReason::TerminalizingControl {
                cause:
                    crate::runtime_surface::TerminalizationCause::HostShutdown
                    | crate::runtime_surface::TerminalizationCause::ThreadClose,
            }
            | crate::runtime_surface::GoalContinuationStopReason::PendingInteraction { .. }
            | crate::runtime_surface::GoalContinuationStopReason::RuntimeFailure { .. } => {
                GoalPauseReason::Infrastructure
            }
            crate::runtime_surface::GoalContinuationStopReason::WorkflowOwned { .. } => {
                GoalPauseReason::WaitingForWorkflow
            }
            crate::runtime_surface::GoalContinuationStopReason::BudgetLimited { .. } => {
                GoalPauseReason::UsageLimit
            }
            crate::runtime_surface::GoalContinuationStopReason::GoalInactive { state } => {
                match state {
                    crate::runtime_surface::SurfaceGoalState::Paused { reason, .. } => match reason
                    {
                        crate::runtime_surface::SurfaceGoalPauseReason::User => {
                            GoalPauseReason::User
                        }
                        crate::runtime_surface::SurfaceGoalPauseReason::Backoff => {
                            GoalPauseReason::Backoff
                        }
                        crate::runtime_surface::SurfaceGoalPauseReason::Infrastructure => {
                            GoalPauseReason::Infrastructure
                        }
                        crate::runtime_surface::SurfaceGoalPauseReason::WaitingForWorkflow => {
                            GoalPauseReason::WaitingForWorkflow
                        }
                        crate::runtime_surface::SurfaceGoalPauseReason::Recovery => {
                            GoalPauseReason::Recovery
                        }
                        crate::runtime_surface::SurfaceGoalPauseReason::UsageLimit => {
                            GoalPauseReason::UsageLimit
                        }
                        crate::runtime_surface::SurfaceGoalPauseReason::NoProgress => {
                            GoalPauseReason::NoProgress
                        }
                    },
                    _ => GoalPauseReason::NoProgress,
                }
            }
            crate::runtime_surface::GoalContinuationStopReason::PredecessorNotSuccessful {
                terminal,
                ..
            } => match terminal {
                crate::runtime_surface::OperationTerminal::Cancelled {
                    reason:
                        crate::runtime_surface::CancelReason::User
                        | crate::runtime_surface::CancelReason::GoalPause,
                } => GoalPauseReason::User,
                crate::runtime_surface::OperationTerminal::Shutdown { .. } => {
                    GoalPauseReason::Infrastructure
                }
                _ => GoalPauseReason::NoProgress,
            },
            crate::runtime_surface::GoalContinuationStopReason::PlanModeDisallowsContinuation
            | crate::runtime_surface::GoalContinuationStopReason::VerificationPending => {
                GoalPauseReason::NoProgress
            }
        };
        let decision_state = match &input.stop_reason {
            crate::runtime_surface::GoalContinuationStopReason::GoalInactive { state } => {
                state.clone()
            }
            _ => crate::runtime_surface::SurfaceGoalState::Paused {
                reason: match pause_reason {
                    GoalPauseReason::User => crate::runtime_surface::SurfaceGoalPauseReason::User,
                    GoalPauseReason::NoProgress => {
                        crate::runtime_surface::SurfaceGoalPauseReason::NoProgress
                    }
                    GoalPauseReason::Backoff => {
                        crate::runtime_surface::SurfaceGoalPauseReason::Backoff
                    }
                    GoalPauseReason::Infrastructure => {
                        crate::runtime_surface::SurfaceGoalPauseReason::Infrastructure
                    }
                    GoalPauseReason::WaitingForWorkflow => {
                        crate::runtime_surface::SurfaceGoalPauseReason::WaitingForWorkflow
                    }
                    GoalPauseReason::Recovery => {
                        crate::runtime_surface::SurfaceGoalPauseReason::Recovery
                    }
                    GoalPauseReason::UsageLimit => {
                        crate::runtime_surface::SurfaceGoalPauseReason::UsageLimit
                    }
                },
                message: crate::runtime_surface::DisplayText::new(input.pause_message.trim()),
            },
        };
        let next_state = core_goal_state_from_surface(&decision_state);
        transaction.execute(
            "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
            params![
                state_json(&next_state)?,
                input.finished_at,
                input.expected_goal_id.as_str()
            ],
        )?;
        transaction.execute(
            "UPDATE goal_runs
             SET status = 'paused', finished_at = ?1
             WHERE goal_run_id = ?2 AND goal_id = ?3 AND finished_at IS NULL",
            params![
                input.finished_at,
                current_run.goal_run_id.as_str(),
                input.expected_goal_id.as_str()
            ],
        )?;
        if previous_state != next_state {
            insert_transition(
                &transaction,
                &input.expected_goal_id,
                Some(input.identity.goal_outer_turn_id.as_str()),
                &previous_state,
                &next_state,
                "surface_continuation_stopped",
                input.finished_at,
            )?;
        }
        let decision = crate::runtime_surface::GoalContinuationDecision::Stopped {
            reason: input.stop_reason,
            outer_turn_count: input.identity.outer_turn_count,
            goal_state: decision_state,
            terminal: input.terminal,
        };
        let decision_record = load_stored_goal(&transaction, session_id)?
            .expect("stopped surface Goal remains readable")
            .record;
        let decision_revision = decision_previous_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let decision_mutation = GoalSurfaceMutation::ContinuationStopped {
            previous_revision: decision_previous_revision,
            predecessor: input.identity,
            decision: Box::new(decision),
        };
        let retained_decision_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..decision_context.clone()
        };
        let decision_receipt = goal_surface_receipt(
            &retained_decision_context,
            session_id,
            &decision_mutation,
            state.goal_id,
            decision_revision,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(decision_record),
        )?;
        let decision_record = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation: decision_mutation,
            receipt: decision_receipt,
        };
        persist_surface_mutation(
            &transaction,
            &decision_context,
            &decision_record,
            input.finished_at,
        )?;
        persist_surface_state(&transaction, &decision_record)?;
        transaction.commit()?;
        outputs.push(decision_record);
        Ok(outputs)
    }

    pub(crate) fn pause_goal_for_surface(
        &self,
        input: PauseGoalForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_surface_mutation_context(&context)?;
        if input.session_id.trim().is_empty() || input.message.trim().is_empty() {
            return Err(GoalStoreError::Invalid(
                "Goal surface pause input is invalid".to_string(),
            ));
        }
        let session_id = input.session_id.trim();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        if !state.row_present
            || state.goal_id != input.expected_goal_id
            || state.goal_revision != input.expected_goal_revision
        {
            return Err(GoalStoreError::Invalid(
                "Goal surface pause fence is stale".to_string(),
            ));
        }
        let current = load_stored_goal(&transaction, session_id)?
            .ok_or_else(|| GoalStoreError::Invalid("goal disappeared before pause".to_string()))?;
        let current_run = current.record.current_run.as_ref().ok_or_else(|| {
            GoalStoreError::Invalid("Goal surface pause requires a current run".to_string())
        })?;
        let stored_operation_id: String = transaction.query_row(
            "SELECT surface_operation_id FROM goal_runs
             WHERE goal_run_id = ?1 AND goal_id = ?2 AND finished_at IS NULL",
            params![
                current_run.goal_run_id.as_str(),
                input.expected_goal_id.as_str()
            ],
            |row| row.get(0),
        )?;
        if stored_operation_id
            != uuid::Uuid::from_bytes(*input.expected_operation_id.as_bytes()).to_string()
            || matches!(current.record.state, GoalState::Complete { .. })
        {
            return Err(GoalStoreError::Invalid(
                "Goal surface pause operation binding is stale".to_string(),
            ));
        }
        let previous_state = current.record.state.clone();
        let next_state = GoalState::Paused {
            reason: GoalPauseReason::User,
            message: input.message.trim().to_string(),
        };
        transaction.execute(
            "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
            params![
                state_json(&next_state)?,
                input.paused_at,
                input.expected_goal_id.as_str()
            ],
        )?;
        if previous_state != next_state {
            insert_transition(
                &transaction,
                &input.expected_goal_id,
                current_run
                    .outer_turn_id
                    .as_ref()
                    .map(GoalOuterTurnId::as_str),
                &previous_state,
                &next_state,
                "surface_goal_paused",
                input.paused_at,
            )?;
        }
        let paused = load_stored_goal(&transaction, session_id)?
            .expect("paused surface Goal remains readable")
            .record;
        let next_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let mutation = GoalSurfaceMutation::Paused {
            previous_revision: state.goal_revision,
            goal_run_id: current_run.goal_run_id.clone(),
            operation_id: input.expected_operation_id,
            outer_turn_id: current_run.outer_turn_id.clone(),
            message: input.message.trim().to_string(),
        };
        let retained_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..context.clone()
        };
        let receipt = goal_surface_receipt(
            &retained_context,
            session_id,
            &mutation,
            state.goal_id,
            next_revision,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(paused),
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, input.paused_at)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub(crate) fn pause_quiescent_goal_for_surface(
        &self,
        input: PauseQuiescentGoalForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_surface_mutation_context(&context)?;
        if input.session_id.trim().is_empty() || input.message.trim().is_empty() {
            return Err(GoalStoreError::Invalid(
                "Goal surface quiescent pause input is invalid".to_string(),
            ));
        }
        let session_id = input.session_id.trim();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        if !state.row_present
            || state.goal_id != input.expected_goal_id
            || state.goal_revision != input.expected_goal_revision
        {
            return Err(GoalStoreError::Invalid(
                "Goal surface quiescent pause fence is stale".to_string(),
            ));
        }
        let current = load_stored_goal(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal disappeared before quiescent pause".to_string())
        })?;
        if current.record.current_run.is_some()
            || matches!(current.record.state, GoalState::Complete { .. })
        {
            return Err(GoalStoreError::Invalid(
                "Goal surface quiescent pause requires an open non-running Goal".to_string(),
            ));
        }
        let previous_state = current.record.state.clone();
        let next_state = GoalState::Paused {
            reason: GoalPauseReason::User,
            message: input.message.trim().to_string(),
        };
        transaction.execute(
            "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
            params![
                state_json(&next_state)?,
                input.paused_at,
                input.expected_goal_id.as_str()
            ],
        )?;
        if previous_state != next_state {
            insert_transition(
                &transaction,
                &input.expected_goal_id,
                None,
                &previous_state,
                &next_state,
                "surface_goal_paused",
                input.paused_at,
            )?;
        }
        let paused = load_stored_goal(&transaction, session_id)?
            .expect("paused quiescent surface Goal remains readable")
            .record;
        let next_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let mutation = GoalSurfaceMutation::PausedQuiescent {
            previous_revision: state.goal_revision,
            message: input.message.trim().to_string(),
        };
        let retained_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..context.clone()
        };
        let receipt = goal_surface_receipt(
            &retained_context,
            session_id,
            &mutation,
            state.goal_id,
            next_revision,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(paused),
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, input.paused_at)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub(crate) fn recover_goal_run_for_surface(
        &self,
        input: RecoverGoalRunForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_surface_mutation_context(&context)?;
        let session_id = input.session_id.trim();
        if session_id.is_empty() || input.recovery_message.trim().is_empty() {
            return Err(GoalStoreError::Invalid(
                "Goal recovery requires a session and message".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        if !state.row_present
            || state.goal_id != input.expected_goal_id
            || state.goal_revision != input.expected_goal_revision
        {
            return Err(GoalStoreError::Invalid(
                "goal surface fence is stale".to_string(),
            ));
        }
        let stored = load_stored_goal(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal disappeared before surface recovery".to_string())
        })?;
        let current_run = stored.record.current_run.as_ref().ok_or_else(|| {
            GoalStoreError::Invalid("Goal recovery requires an open run".to_string())
        })?;
        let operation_json: String = transaction.query_row(
            "SELECT surface_operation_json
             FROM goal_runs
             WHERE goal_run_id = ?1 AND finished_at IS NULL",
            [current_run.goal_run_id.as_str()],
            |row| row.get(0),
        )?;
        let operation: OperationRecord = serde_json::from_str(&operation_json)?;
        validate_surface_goal_operation(
            &operation,
            &input.expected_goal_id,
            &current_run.goal_run_id,
            state.objective_revision,
        )?;
        match (&input.stale_identity, current_run.in_flight) {
            (Some(identity), true)
                if identity.goal_id.as_str() == input.expected_goal_id.as_str()
                    && identity.goal_run_id.as_str() == current_run.goal_run_id.as_str()
                    && identity.operation_fence.operation_id == operation.operation_id
                    && identity.objective_revision.get() == state.objective_revision
                    && current_run
                        .outer_turn_id
                        .as_ref()
                        .is_some_and(|outer_turn_id| {
                            outer_turn_id.as_str() == identity.goal_outer_turn_id.as_str()
                        }) => {}
            (None, false) if current_run.outer_turn_id.is_none() => {}
            _ => {
                return Err(GoalStoreError::Invalid(
                    "Goal recovery identity does not match the durable open run".to_string(),
                ));
            }
        }
        let stale_goal_run_id = current_run.goal_run_id.clone();
        let stale_origin = current_run.origin;
        let previous_state = stored.record.state;
        let next_state = GoalState::Paused {
            reason: GoalPauseReason::Recovery,
            message: input.recovery_message.trim().to_string(),
        };
        transaction.execute(
            "UPDATE goal_runs
             SET status = 'recovered', in_flight = 0, finished_at = ?1
             WHERE goal_run_id = ?2 AND finished_at IS NULL",
            params![input.recovered_at, stale_goal_run_id.as_str()],
        )?;
        transaction.execute(
            "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
            params![
                state_json(&next_state)?,
                input.recovered_at,
                input.expected_goal_id.as_str()
            ],
        )?;
        insert_transition(
            &transaction,
            &input.expected_goal_id,
            None,
            &previous_state,
            &next_state,
            "surface_recovered",
            input.recovered_at,
        )?;
        let record = load_stored_goal(&transaction, session_id)?
            .expect("surface-recovered Goal remains readable")
            .record;
        let goal_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let mutation = GoalSurfaceMutation::Recovered {
            previous_revision: state.goal_revision,
            stale_goal_run_id,
            operation: Box::new(operation),
            origin: stale_origin,
            stale_identity: input.stale_identity,
            stale_run_settled: false,
            recovery_message: input.recovery_message.trim().to_string(),
        };
        let retained_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..context.clone()
        };
        let receipt = goal_surface_receipt(
            &retained_context,
            session_id,
            &mutation,
            state.goal_id,
            goal_revision,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(record),
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation,
            receipt,
        };
        persist_surface_mutation(&transaction, &context, &output, input.recovered_at)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub(crate) fn replace_goal_continuation_with_recovery_for_surface(
        &self,
        input: ReplaceGoalContinuationForSurfaceInput,
        context: GoalSurfaceMutationContext,
    ) -> Result<GoalSurfaceMutationRecord, GoalStoreError> {
        validate_surface_mutation_context(&context)?;
        if input.recovery_message.trim().is_empty() {
            return Err(GoalStoreError::Invalid(
                "Goal continuation recovery requires a message".to_string(),
            ));
        }
        let (predecessor, admitted_successor) = match &input.interrupted.mutation {
            GoalSurfaceMutation::ContinuationStopped { predecessor, .. } => (predecessor, None),
            GoalSurfaceMutation::ContinuationAdmitted {
                predecessor,
                decision,
                ..
            } => {
                let crate::runtime_surface::GoalContinuationDecision::Admitted {
                    successor, ..
                } = decision.as_ref()
                else {
                    return Err(GoalStoreError::Invalid(
                        "admitted Goal continuation carries a stopped decision".to_string(),
                    ));
                };
                (predecessor, Some(successor))
            }
            _ => {
                return Err(GoalStoreError::Invalid(
                    "Goal continuation recovery requires an interrupted decision".to_string(),
                ));
            }
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, context.goal_owner_epoch)?;
        if let Some(replay) = replay_surface_mutation(&transaction, &context)? {
            transaction.commit()?;
            return Ok(replay);
        }
        let stored_interrupted = transaction
            .query_row(
                "SELECT session_id, store_commit_id, command_digest, receipt_digest, payload_json
                 FROM goal_surface_outbox
                 WHERE store_commit_id = ?1 AND acknowledged = 0",
                [&input.interrupted.receipt.store_commit_id],
                |row| {
                    Ok(StoredSurfaceMutation {
                        session_id: row.get(0)?,
                        store_commit_id: row.get(1)?,
                        command_digest: row.get(2)?,
                        receipt_digest: row.get(3)?,
                        payload_json: row.get(4)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| {
                GoalStoreError::Invalid(
                    "interrupted Goal continuation is no longer pending".to_string(),
                )
            })?;
        let (_, canonical_interrupted) = validate_stored_surface_mutation(&stored_interrupted)?;
        if canonical_interrupted != input.interrupted {
            return Err(GoalStoreError::Invalid(
                "interrupted Goal continuation receipt changed".to_string(),
            ));
        }
        let session_id = input.interrupted.session_id.as_str();
        let state = load_goal_surface_state(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal surface state does not exist".to_string())
        })?;
        let last_receipt_digest: Vec<u8> = transaction.query_row(
            "SELECT last_receipt_digest FROM goal_surface_state WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        if state.goal_revision != input.interrupted.receipt.goal_revision
            || exact_digest(&last_receipt_digest, "receipt digest")?
                != input.interrupted.receipt.receipt_digest
            || state.goal_id != input.interrupted.receipt.goal_id
            || input.surface_previous_revision >= input.interrupted.receipt.goal_revision
        {
            return Err(GoalStoreError::Invalid(
                "interrupted Goal continuation is not the current durable state".to_string(),
            ));
        }
        let stored = load_stored_goal(&transaction, session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("goal disappeared before continuation recovery".to_string())
        })?;
        match (stored.record.current_run.as_ref(), admitted_successor) {
            (None, None) => {}
            (Some(run), Some(successor))
                if run.goal_run_id.as_str() == successor.goal_run_id.as_str()
                    && run.in_flight
                    && run.outer_turn_id.as_ref().is_some_and(|outer_turn_id| {
                        outer_turn_id.as_str() == successor.goal_outer_turn_id.as_str()
                    }) =>
            {
                transaction.execute(
                    "UPDATE goal_runs
                     SET status = 'recovered', current_outer_turn_id = NULL,
                         in_flight = 0, finished_at = ?1
                     WHERE goal_run_id = ?2 AND goal_id = ?3
                       AND finished_at IS NULL AND in_flight = 1",
                    params![
                        input.recovered_at,
                        predecessor.goal_run_id.as_str(),
                        input.interrupted.receipt.goal_id.as_str()
                    ],
                )?;
                transaction.execute(
                    "UPDATE goal_turns
                     SET status = 'recovered', finished_at = ?1
                     WHERE outer_turn_id = ?2 AND goal_run_id = ?3
                       AND status = 'in_flight'",
                    params![
                        input.recovered_at,
                        successor.goal_outer_turn_id.as_str(),
                        predecessor.goal_run_id.as_str()
                    ],
                )?;
            }
            (Some(_), None) => {
                return Err(GoalStoreError::Invalid(
                    "interrupted stopped Goal continuation retained an open run".to_string(),
                ));
            }
            _ => {
                return Err(GoalStoreError::Invalid(
                    "interrupted admitted Goal continuation run identity changed".to_string(),
                ));
            }
        }
        let (operation_json, origin): (String, String) = transaction.query_row(
            "SELECT surface_operation_json, origin
             FROM goal_runs
             WHERE goal_run_id = ?1 AND goal_id = ?2",
            params![
                predecessor.goal_run_id.as_str(),
                input.interrupted.receipt.goal_id.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let operation: OperationRecord = serde_json::from_str(&operation_json)?;
        if operation.operation_id != predecessor.operation_fence.operation_id {
            return Err(GoalStoreError::Invalid(
                "interrupted Goal continuation operation identity changed".to_string(),
            ));
        }
        let origin = parse_origin(&origin)?;
        let previous_state = stored.record.state;
        let next_state = GoalState::Paused {
            reason: GoalPauseReason::Recovery,
            message: input.recovery_message.trim().to_string(),
        };
        transaction.execute(
            "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
            params![
                state_json(&next_state)?,
                input.recovered_at,
                input.interrupted.receipt.goal_id.as_str()
            ],
        )?;
        if previous_state != next_state {
            insert_transition(
                &transaction,
                &input.interrupted.receipt.goal_id,
                Some(predecessor.goal_outer_turn_id.as_str()),
                &previous_state,
                &next_state,
                "surface_continuation_recovered",
                input.recovered_at,
            )?;
        }
        let record = load_stored_goal(&transaction, session_id)?
            .expect("recovered Goal remains readable")
            .record;
        let mutation = GoalSurfaceMutation::Recovered {
            previous_revision: input.surface_previous_revision,
            stale_goal_run_id: GoalRunId::parse(predecessor.goal_run_id.as_str().to_string())
                .map_err(GoalStoreError::Invalid)?,
            operation: Box::new(operation),
            origin,
            stale_identity: Some(predecessor.clone()),
            stale_run_settled: input.stale_run_settled,
            recovery_message: input.recovery_message.trim().to_string(),
        };
        let retained_context = GoalSurfaceMutationContext {
            goal_owner_epoch: state.goal_owner_epoch,
            ..context.clone()
        };
        let receipt = goal_surface_receipt(
            &retained_context,
            session_id,
            &mutation,
            state.goal_id,
            input
                .surface_previous_revision
                .checked_add(1)
                .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(record),
        )?;
        let output = GoalSurfaceMutationRecord {
            session_id: session_id.to_string(),
            mutation,
            receipt,
        };
        supersede_goal_continuation_outbox(
            &transaction,
            session_id,
            predecessor.as_ref(),
            &input.interrupted.receipt.store_commit_id,
        )?;
        persist_surface_mutation(&transaction, &context, &output, input.recovered_at)?;
        persist_surface_state(&transaction, &output)?;
        transaction.commit()?;
        Ok(output)
    }

    pub fn pending_surface_mutations(
        &self,
        session_id: &str,
    ) -> Result<Vec<GoalSurfaceMutationRecord>, GoalStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT session_id, store_commit_id, command_digest, receipt_digest, payload_json
             FROM goal_surface_outbox
             WHERE session_id = ?1 AND acknowledged = 0
             ORDER BY sequence ASC",
        )?;
        statement
            .query_map([session_id], |row| {
                Ok(StoredSurfaceMutation {
                    session_id: row.get(0)?,
                    store_commit_id: row.get(1)?,
                    command_digest: row.get(2)?,
                    receipt_digest: row.get(3)?,
                    payload_json: row.get(4)?,
                })
            })?
            .map(|stored| {
                let stored = stored?;
                validate_stored_surface_mutation(&stored).map(|(_, mutation)| mutation)
            })
            .collect()
    }

    pub(crate) fn acknowledge_surface_mutation(
        &self,
        store_commit_id: &str,
        receipt_digest: &[u8; 32],
        goal_owner_epoch: u64,
    ) -> Result<bool, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, goal_owner_epoch)?;
        let stored = transaction
            .query_row(
                "SELECT session_id, store_commit_id, command_digest, receipt_digest, payload_json
                 FROM goal_surface_outbox
                 WHERE store_commit_id = ?1",
                [store_commit_id],
                |row| {
                    Ok(StoredSurfaceMutation {
                        session_id: row.get(0)?,
                        store_commit_id: row.get(1)?,
                        command_digest: row.get(2)?,
                        receipt_digest: row.get(3)?,
                        payload_json: row.get(4)?,
                    })
                },
            )
            .optional()?;
        let Some(stored) = stored else {
            return Ok(false);
        };
        let (_, mutation) = validate_stored_surface_mutation(&stored)?;
        if &mutation.receipt.receipt_digest != receipt_digest {
            return Ok(false);
        }
        let changed = transaction.execute(
            "UPDATE goal_surface_outbox SET acknowledged = 1
             WHERE store_commit_id = ?1",
            [store_commit_id],
        )?;
        let acknowledged: i64 = transaction.query_row(
            "SELECT acknowledged FROM goal_surface_outbox WHERE store_commit_id = ?1",
            [store_commit_id],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        if acknowledged != 1 {
            return Err(GoalStoreError::Invalid(format!(
                "Goal surface acknowledgement did not persist (changed={changed}, value={acknowledged})"
            )));
        }
        Ok(true)
    }

    pub fn get_by_session(&self, session_id: &str) -> Result<Option<GoalRecord>, GoalStoreError> {
        let connection = self.connection()?;
        Ok(load_stored_goal(&connection, session_id)?.map(|stored| stored.record))
    }

    /// Recent gap fingerprints for a goal, most recent first. `None` is a
    /// progress barrier and must be preserved so equal gaps separated by a
    /// productive turn never become one synthetic streak after restart.
    pub fn recent_gap_fingerprints(
        &self,
        goal_id: &GoalId,
        limit: u32,
    ) -> Result<Vec<Option<String>>, GoalStoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT t.gap_fingerprint
             FROM goal_turns t
             JOIN goal_runs r ON t.goal_run_id = r.goal_run_id
             WHERE r.goal_id = ?1
               AND t.finished_at IS NOT NULL
             ORDER BY t.finished_at DESC, t.rowid DESC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![goal_id.as_str(), limit], |row| {
            row.get::<_, Option<String>>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn project_thread_goal(
        &self,
        session_id: &str,
    ) -> Result<Option<ThreadGoal>, GoalStoreError> {
        let connection = self.connection()?;
        let Some(stored) = load_stored_goal(&connection, session_id)? else {
            return Ok(None);
        };
        Ok(Some(ThreadGoal {
            session_id: stored.record.session_id,
            objective: stored.record.objective,
            status: ThreadGoalStatus::from_runtime_state(&stored.record.state),
            token_budget: stored.record.token_budget,
            tokens_used: stored.record.usage.charged_tokens(),
            time_used_seconds: stored.record.usage.elapsed_seconds,
            created_at: stored.created_at,
            updated_at: stored.updated_at,
        }))
    }

    pub fn latest_active(&self) -> Result<Option<ThreadGoal>, GoalStoreError> {
        let connection = self.connection()?;
        let session_id: Option<String> = connection
            .query_row(
                "SELECT session_id FROM goals
                 WHERE state LIKE '{\"status\":\"active\"%'
                 ORDER BY updated_at DESC, created_at DESC, session_id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match session_id.as_deref() {
            Some(session_id) => self.project_thread_goal(session_id),
            None => Ok(None),
        }
    }

    pub fn edit_goal(
        &self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
        updated_at: i64,
    ) -> Result<Option<GoalRecord>, GoalStoreError> {
        validate_thread_goal_objective(objective).map_err(GoalStoreError::Invalid)?;
        if token_budget.is_some_and(|budget| budget <= 0) {
            return Err(GoalStoreError::Invalid(
                "goal token budget must be positive".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_session_not_surface_owned(&transaction, session_id, "edit")?;
        let row: Option<(String, String)> = transaction
            .query_row(
                "SELECT goal_id, state FROM goals WHERE session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((goal_id, previous_json)) = row else {
            transaction.commit()?;
            return Ok(None);
        };
        let previous = parse_state(&previous_json)?;
        let goal_id = GoalId::parse(goal_id).map_err(GoalStoreError::Invalid)?;
        ensure_goal_not_in_flight(&transaction, goal_id.as_str(), "edit")?;
        let next = GoalState::Active;
        transaction.execute(
            "UPDATE goals SET objective = ?1, objective_revision = objective_revision + 1,
                state = ?2, token_budget = ?3, updated_at = ?4 WHERE session_id = ?5",
            params![
                objective.trim(),
                state_json(&next)?,
                token_budget,
                updated_at,
                session_id
            ],
        )?;
        transaction.execute(
            "UPDATE goal_runs
             SET status = 'edited', in_flight = 0, finished_at = COALESCE(finished_at, ?1)
             WHERE goal_id = ?2 AND finished_at IS NULL",
            params![updated_at, goal_id.as_str()],
        )?;
        insert_transition(
            &transaction,
            &goal_id,
            None,
            &previous,
            &next,
            "edited",
            updated_at,
        )?;
        transaction.commit()?;
        self.get_by_session(session_id)
    }

    pub fn resume_into(
        &self,
        source_session_id: &str,
        resumed_session_id: &str,
        now: i64,
    ) -> Result<Option<GoalRecord>, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_session_not_surface_owned(&transaction, source_session_id, "resume")?;
        ensure_session_not_surface_owned(&transaction, resumed_session_id, "resume into")?;
        let source = transaction
            .query_row(
                "SELECT goal_id, objective, token_budget, created_at, state
                 FROM goals WHERE session_id = ?1",
                [source_session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((source_goal_id, objective, token_budget, created_at, source_state_json)) = source
        else {
            transaction.commit()?;
            return Ok(None);
        };
        ensure_goal_not_in_flight(&transaction, &source_goal_id, "resume")?;
        let source_state = parse_state(&source_state_json)?;
        if source_session_id != resumed_session_id
            && transaction
                .query_row(
                    "SELECT 1 FROM goals WHERE session_id = ?1",
                    [resumed_session_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .is_some()
        {
            return Err(GoalStoreError::Invalid(format!(
                "goal already exists for resume target session '{resumed_session_id}'"
            )));
        }
        if source_session_id == resumed_session_id {
            let goal_id = GoalId::parse(source_goal_id).map_err(GoalStoreError::Invalid)?;
            let next = GoalState::Active;
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE session_id = ?3",
                params![state_json(&next)?, now, resumed_session_id],
            )?;
            insert_transition(
                &transaction,
                &goal_id,
                None,
                &source_state,
                &next,
                "resumed",
                now,
            )?;
        } else {
            let source_goal_id = GoalId::parse(source_goal_id).map_err(GoalStoreError::Invalid)?;
            let usage = usage_totals(&transaction, &source_goal_id)?;
            let next_goal_id = GoalId::new();
            let next = GoalState::Active;
            let paused = GoalState::Paused {
                reason: GoalPauseReason::User,
                message: format!("paused while resuming into session {resumed_session_id}"),
            };
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&paused)?, now, source_goal_id.as_str()],
            )?;
            transaction.execute(
                "UPDATE goal_runs
                 SET status = 'resumed_elsewhere', in_flight = 0,
                     finished_at = COALESCE(finished_at, ?1)
                 WHERE goal_id = ?2 AND finished_at IS NULL",
                params![now, source_goal_id.as_str()],
            )?;
            insert_transition(
                &transaction,
                &source_goal_id,
                None,
                &source_state,
                &paused,
                "resume_fork_source_paused",
                now,
            )?;
            transaction.execute(
                "INSERT INTO goals (
                    goal_id, session_id, objective, objective_revision, state,
                    token_budget, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
                params![
                    next_goal_id.as_str(),
                    resumed_session_id,
                    objective,
                    state_json(&next)?,
                    token_budget,
                    created_at,
                    now
                ],
            )?;
            insert_transition(
                &transaction,
                &next_goal_id,
                None,
                &next,
                &next,
                "resumed",
                now,
            )?;
            if usage.charged_tokens() > 0 || usage.elapsed_seconds > 0 {
                transaction.execute(
                    "INSERT INTO goal_usage_events (
                        usage_event_id, goal_id, source, charged_input_tokens,
                        output_tokens, cache_tokens, verifier_tokens, cost_micros,
                        elapsed_seconds, created_at
                     ) VALUES (?1, ?2, 'resume_copy', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        format!("resume:{source_goal_id}:{resumed_session_id}"),
                        next_goal_id.as_str(),
                        usage.charged_input_tokens,
                        usage.output_tokens,
                        usage.cache_tokens,
                        usage.verifier_tokens,
                        usage.cost_micros,
                        usage.elapsed_seconds,
                        now
                    ],
                )?;
            }
        }
        transaction.commit()?;
        self.get_by_session(resumed_session_id)
    }

    pub fn begin_run(&self, input: BeginGoalRunInput) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_goal_not_surface_owned(&transaction, &input.goal_id, "begin a run for")?;
        let state = goal_state_by_id(&transaction, &input.goal_id)?;
        if !state.should_continue() {
            return Err(GoalStoreError::Invalid(format!(
                "cannot begin goal run while state is {state:?}"
            )));
        }
        transaction.execute(
            "INSERT INTO goal_runs (
                goal_run_id, goal_id, status, origin, current_outer_turn_id,
                continuation_count, in_flight, started_at, finished_at
             ) VALUES (?1, ?2, 'active', ?3, NULL, 0, 0, ?4, NULL)",
            params![
                input.goal_run_id.as_str(),
                input.goal_id.as_str(),
                origin_name(input.origin),
                input.started_at,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn begin_outer_turn(&self, input: BeginOuterTurnInput) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_goal_not_surface_owned(&transaction, &input.goal_id, "begin an outer turn for")?;
        let changed = transaction.execute(
            "UPDATE goal_runs
             SET current_outer_turn_id = ?1,
                 continuation_count = continuation_count + 1,
                 in_flight = 1
             WHERE goal_run_id = ?2 AND goal_id = ?3 AND in_flight = 0 AND finished_at IS NULL",
            params![
                input.outer_turn_id.as_str(),
                input.goal_run_id.as_str(),
                input.goal_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(GoalStoreError::Invalid(
                "goal run is missing, stale, or already has an in-flight outer turn".to_string(),
            ));
        }
        transaction.execute(
            "INSERT INTO goal_turns (
                outer_turn_id, goal_run_id, origin, provider_turn_id, status,
                tool_count, model_response_count, charged_input_tokens,
                output_tokens, verifier_tokens, gap_fingerprint, started_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, 'in_flight', 0, 0, 0, 0, 0, NULL, ?5, NULL)",
            params![
                input.outer_turn_id.as_str(),
                input.goal_run_id.as_str(),
                origin_name(input.origin),
                input.provider_turn_id,
                input.started_at,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_usage_once(&self, event: GoalUsageEvent) -> Result<GoalUsage, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_goal_not_surface_owned(&transaction, &event.goal_id, "record usage for")?;
        transaction.execute(
            "INSERT OR IGNORE INTO goal_usage_events (
                usage_event_id, goal_id, source, charged_input_tokens,
                output_tokens, cache_tokens, verifier_tokens, cost_micros,
                elapsed_seconds, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.usage_event_id,
                event.goal_id.as_str(),
                event.source,
                event.usage.charged_input_tokens.max(0),
                event.usage.output_tokens.max(0),
                event.usage.cache_tokens.max(0),
                event.usage.verifier_tokens.max(0),
                event.usage.cost_micros.max(0),
                event.usage.elapsed_seconds.max(0),
                event.created_at,
            ],
        )?;
        let usage = usage_totals(&transaction, &event.goal_id)?;
        let (state, token_budget) = transaction.query_row(
            "SELECT state, token_budget FROM goals WHERE goal_id = ?1",
            [event.goal_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        let state = parse_state(&state)?;
        if state.should_continue()
            && token_budget.is_some_and(|budget| usage.charged_tokens() >= budget)
        {
            let next = GoalState::BudgetLimited;
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&next)?, event.created_at, event.goal_id.as_str()],
            )?;
            insert_transition(
                &transaction,
                &event.goal_id,
                None,
                &state,
                &next,
                "budget_limited",
                event.created_at,
            )?;
        }
        transaction.commit()?;
        Ok(usage)
    }

    pub fn record_verifier_usage_once(
        &self,
        outer_turn_id: &GoalOuterTurnId,
        event: GoalUsageEvent,
    ) -> Result<GoalUsage, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_goal_not_surface_owned(&transaction, &event.goal_id, "record verifier usage for")?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO goal_usage_events (
                usage_event_id, goal_id, source, charged_input_tokens,
                output_tokens, cache_tokens, verifier_tokens, cost_micros,
                elapsed_seconds, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.usage_event_id,
                event.goal_id.as_str(),
                event.source,
                event.usage.charged_input_tokens.max(0),
                event.usage.output_tokens.max(0),
                event.usage.cache_tokens.max(0),
                event.usage.verifier_tokens.max(0),
                event.usage.cost_micros.max(0),
                event.usage.elapsed_seconds.max(0),
                event.created_at,
            ],
        )?;
        if inserted == 1 {
            let changed = transaction.execute(
                "UPDATE goal_turns
                 SET verifier_tokens = verifier_tokens + ?1
                 WHERE outer_turn_id = ?2
                   AND finished_at IS NOT NULL
                   AND goal_run_id IN (
                       SELECT goal_run_id FROM goal_runs WHERE goal_id = ?3
                   )",
                params![
                    event.usage.verifier_tokens.max(0),
                    outer_turn_id.as_str(),
                    event.goal_id.as_str(),
                ],
            )?;
            if changed != 1 {
                return Err(GoalStoreError::Invalid(
                    "verifier usage references a missing, in-flight, or unrelated outer turn"
                        .to_string(),
                ));
            }
        }
        let usage = usage_totals(&transaction, &event.goal_id)?;
        let (state, token_budget) = transaction.query_row(
            "SELECT state, token_budget FROM goals WHERE goal_id = ?1",
            [event.goal_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        let state = parse_state(&state)?;
        if state.should_continue()
            && token_budget.is_some_and(|budget| usage.charged_tokens() >= budget)
        {
            let next = GoalState::BudgetLimited;
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&next)?, event.created_at, event.goal_id.as_str()],
            )?;
            insert_transition(
                &transaction,
                &event.goal_id,
                Some(outer_turn_id.as_str()),
                &state,
                &next,
                "budget_limited",
                event.created_at,
            )?;
        }
        transaction.commit()?;
        Ok(usage)
    }

    pub fn record_intent(&self, record: GoalIntentRecord) -> Result<GoalUpdateAck, GoalStoreError> {
        self.record_intent_with_owner(record, false)
    }

    pub(crate) fn record_intent_for_surface(
        &self,
        record: GoalIntentRecord,
        identity: Box<SurfaceGoalGenerationIdentity>,
        contexts: [GoalSurfaceMutationContext; 2],
    ) -> Result<(GoalUpdateAck, Vec<GoalSurfaceMutationRecord>), GoalStoreError> {
        let [requested_context, acknowledged_context] = contexts;
        validate_surface_mutation_context(&requested_context)?;
        validate_surface_mutation_context(&acknowledged_context)?;
        if requested_context.store_commit_id == acknowledged_context.store_commit_id
            || requested_context.goal_owner_epoch != acknowledged_context.goal_owner_epoch
        {
            return Err(GoalStoreError::Invalid(
                "Goal intent surface contexts are invalid".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_surface_owner_epoch(&transaction, requested_context.goal_owner_epoch)?;
        ensure_outer_turn_surface_owned(&transaction, &record.outer_turn_id)?;
        if identity.goal_outer_turn_id.as_str() != record.outer_turn_id.as_str() {
            return Err(GoalStoreError::Invalid(
                "Goal intent identity does not match its outer turn".to_string(),
            ));
        }
        let replay_requested = replay_surface_mutation(&transaction, &requested_context)?;
        let replay_acknowledged = replay_surface_mutation(&transaction, &acknowledged_context)?;
        match (replay_requested, replay_acknowledged) {
            (Some(requested), Some(acknowledged)) => {
                let ack = match &acknowledged.mutation {
                    GoalSurfaceMutation::IntentAcknowledged { ack, .. } => ack.clone(),
                    _ => {
                        return Err(GoalStoreError::Invalid(
                            "Goal intent replay has the wrong mutation kind".to_string(),
                        ));
                    }
                };
                transaction.commit()?;
                return Ok((ack, vec![requested, acknowledged]));
            }
            (None, None) => {}
            _ => {
                return Err(GoalStoreError::Invalid(
                    "Goal intent surface replay is incomplete".to_string(),
                ));
            }
        }
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO goal_intents (
                intent_id, outer_turn_id, requested_state, payload_json,
                ack_code, ack_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.intent.intent_id.as_str(),
                record.outer_turn_id.as_str(),
                requested_state_name(record.intent.requested_state),
                serde_json::to_string(&record.intent)?,
                ack_code(&record.ack),
                serde_json::to_string(&record.ack)?,
                record.created_at,
            ],
        )?;
        let ack: GoalUpdateAck = if inserted == 1 {
            record.ack.clone()
        } else {
            let ack_json: String = transaction.query_row(
                "SELECT ack_json FROM goal_intents WHERE intent_id = ?1",
                [record.intent.intent_id.as_str()],
                |row| row.get(0),
            )?;
            serde_json::from_str(&ack_json)?
        };
        let session_id: String = transaction.query_row(
            "SELECT goals.session_id
             FROM goal_turns AS turns
             JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
             JOIN goals ON goals.goal_id = runs.goal_id
             WHERE turns.outer_turn_id = ?1
               AND turns.status = 'in_flight'
               AND runs.in_flight = 1",
            [record.outer_turn_id.as_str()],
            |row| row.get(0),
        )?;
        let state = load_goal_surface_state(&transaction, &session_id)?.ok_or_else(|| {
            GoalStoreError::Invalid("Goal intent has no surface state".to_string())
        })?;
        let goal = load_stored_goal(&transaction, &session_id)?
            .ok_or_else(|| GoalStoreError::Invalid("Goal intent lost its Goal".to_string()))?
            .record;
        let requested_revision = state
            .goal_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let requested_mutation = GoalSurfaceMutation::IntentRequested {
            previous_revision: state.goal_revision,
            identity: identity.clone(),
            intent: record.intent.clone(),
        };
        let requested_receipt = goal_surface_receipt(
            &GoalSurfaceMutationContext {
                goal_owner_epoch: state.goal_owner_epoch,
                ..requested_context.clone()
            },
            &session_id,
            &requested_mutation,
            state.goal_id.clone(),
            requested_revision,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(goal.clone()),
        )?;
        let requested = GoalSurfaceMutationRecord {
            session_id: session_id.clone(),
            mutation: requested_mutation,
            receipt: requested_receipt,
        };
        persist_surface_mutation(
            &transaction,
            &requested_context,
            &requested,
            record.created_at,
        )?;
        persist_surface_state(&transaction, &requested)?;
        let acknowledged_revision = requested_revision
            .checked_add(1)
            .ok_or_else(|| GoalStoreError::Invalid("goal revision exhausted".to_string()))?;
        let acknowledged_mutation = GoalSurfaceMutation::IntentAcknowledged {
            previous_revision: requested_revision,
            identity,
            intent: record.intent,
            ack: ack.clone(),
        };
        let acknowledged_receipt = goal_surface_receipt(
            &GoalSurfaceMutationContext {
                goal_owner_epoch: state.goal_owner_epoch,
                ..acknowledged_context.clone()
            },
            &session_id,
            &acknowledged_mutation,
            state.goal_id,
            acknowledged_revision,
            state.objective_revision,
            state.catalog_revision,
            GoalSurfaceRowState::Present(goal),
        )?;
        let acknowledged = GoalSurfaceMutationRecord {
            session_id,
            mutation: acknowledged_mutation,
            receipt: acknowledged_receipt,
        };
        persist_surface_mutation(
            &transaction,
            &acknowledged_context,
            &acknowledged,
            record.created_at,
        )?;
        persist_surface_state(&transaction, &acknowledged)?;
        transaction.commit()?;
        Ok((ack, vec![requested, acknowledged]))
    }

    fn record_intent_with_owner(
        &self,
        record: GoalIntentRecord,
        surface_owned: bool,
    ) -> Result<GoalUpdateAck, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if surface_owned {
            ensure_outer_turn_surface_owned(&transaction, &record.outer_turn_id)?;
        } else {
            ensure_outer_turn_not_surface_owned(
                &transaction,
                &record.outer_turn_id,
                "record an intent for",
            )?;
        }
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO goal_intents (
                intent_id, outer_turn_id, requested_state, payload_json,
                ack_code, ack_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.intent.intent_id.as_str(),
                record.outer_turn_id.as_str(),
                requested_state_name(record.intent.requested_state),
                serde_json::to_string(&record.intent)?,
                ack_code(&record.ack),
                serde_json::to_string(&record.ack)?,
                record.created_at,
            ],
        )?;
        let ack_json: String = if inserted == 1 {
            serde_json::to_string(&record.ack)?
        } else {
            transaction.query_row(
                "SELECT ack_json FROM goal_intents WHERE intent_id = ?1",
                [record.intent.intent_id.as_str()],
                |row| row.get(0),
            )?
        };
        transaction.commit()?;
        Ok(serde_json::from_str(&ack_json)?)
    }

    pub fn intent_count(&self) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row("SELECT COUNT(*) FROM goal_intents", [], |row| row.get(0))?)
    }

    pub fn finish_outer_turn(
        &self,
        input: FinishOuterTurnInput,
    ) -> Result<FinishOuterTurnOutcome, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_goal_not_surface_owned(&transaction, &input.goal_id, "finish an outer turn for")?;
        let status: Option<String> = transaction
            .query_row(
                "SELECT status FROM goal_turns
                 WHERE outer_turn_id = ?1 AND goal_run_id = ?2",
                params![input.outer_turn_id.as_str(), input.goal_run_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Err(GoalStoreError::Invalid(
                "goal outer turn does not exist".to_string(),
            ));
        };
        if status != "in_flight" {
            let usage = usage_totals(&transaction, &input.goal_id)?;
            transaction.commit()?;
            return Ok(FinishOuterTurnOutcome {
                already_finished: true,
                usage,
            });
        }
        let turn_usage = input
            .usage_event
            .as_ref()
            .map(|event| event.usage.clone())
            .unwrap_or_default();
        if let Some(event) = input.usage_event {
            insert_usage_event(&transaction, &event)?;
        }
        let changed = transaction.execute(
            "UPDATE goal_turns SET status = ?1, tool_count = ?2,
                model_response_count = ?3, charged_input_tokens = ?4,
                output_tokens = ?5, verifier_tokens = ?6,
                gap_fingerprint = ?7, finished_at = ?8
             WHERE outer_turn_id = ?9 AND goal_run_id = ?10 AND status = 'in_flight'",
            params![
                turn_status_name(input.status),
                input.tool_count,
                input.model_response_count,
                turn_usage.charged_input_tokens.max(0),
                turn_usage.output_tokens.max(0),
                turn_usage.verifier_tokens.max(0),
                input.gap_fingerprint,
                input.finished_at,
                input.outer_turn_id.as_str(),
                input.goal_run_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(GoalStoreError::Invalid(
                "goal outer turn was concurrently finalized".to_string(),
            ));
        }
        transaction.execute(
            "UPDATE goal_runs SET current_outer_turn_id = NULL, in_flight = 0
             WHERE goal_run_id = ?1 AND goal_id = ?2",
            params![input.goal_run_id.as_str(), input.goal_id.as_str()],
        )?;
        let usage = usage_totals(&transaction, &input.goal_id)?;
        let (state_json_value, token_budget): (String, Option<i64>) = transaction.query_row(
            "SELECT state, token_budget FROM goals WHERE goal_id = ?1",
            [input.goal_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let state = parse_state(&state_json_value)?;
        if state.should_continue()
            && token_budget.is_some_and(|budget| usage.charged_tokens() >= budget)
        {
            let next = GoalState::BudgetLimited;
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![
                    state_json(&next)?,
                    input.finished_at,
                    input.goal_id.as_str()
                ],
            )?;
            insert_transition(
                &transaction,
                &input.goal_id,
                Some(input.outer_turn_id.as_str()),
                &state,
                &next,
                "budget_limited",
                input.finished_at,
            )?;
        }
        let final_state = goal_state_by_id(&transaction, &input.goal_id)?;
        if let Some(run_status) = closed_run_status(&final_state) {
            transaction.execute(
                "UPDATE goal_runs
                 SET status = ?1, in_flight = 0, finished_at = COALESCE(finished_at, ?2)
                 WHERE goal_run_id = ?3 AND goal_id = ?4",
                params![
                    run_status,
                    input.finished_at,
                    input.goal_run_id.as_str(),
                    input.goal_id.as_str()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(FinishOuterTurnOutcome {
            already_finished: false,
            usage,
        })
    }

    pub fn outer_turn_status(
        &self,
        outer_turn_id: &GoalOuterTurnId,
    ) -> Result<Option<String>, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                "SELECT status FROM goal_turns WHERE outer_turn_id = ?1",
                [outer_turn_id.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn outer_turn_verifier_tokens(
        &self,
        outer_turn_id: &GoalOuterTurnId,
    ) -> Result<Option<i64>, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                "SELECT verifier_tokens FROM goal_turns WHERE outer_turn_id = ?1",
                [outer_turn_id.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn transition_state(
        &self,
        goal_id: &GoalId,
        next: GoalState,
        reason_code: &str,
        outer_turn_id: Option<&GoalOuterTurnId>,
        updated_at: i64,
    ) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_goal_not_surface_owned(&transaction, goal_id, "transition")?;
        let previous = goal_state_by_id(&transaction, goal_id).map_err(|error| match error {
            GoalStoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                GoalStoreError::Invalid("goal does not exist".to_string())
            }
            error => error,
        })?;
        if matches!(previous, GoalState::Complete { .. }) && previous != next {
            return Err(GoalStoreError::Invalid(
                "complete goal cannot be downgraded by a runtime transition".to_string(),
            ));
        }
        if previous != next {
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&next)?, updated_at, goal_id.as_str()],
            )?;
        }
        if let Some(run_status) = closed_run_status(&next) {
            transaction.execute(
                "UPDATE goal_runs
                 SET status = ?1, in_flight = 0, finished_at = COALESCE(finished_at, ?2)
                 WHERE goal_id = ?3 AND finished_at IS NULL",
                params![run_status, updated_at, goal_id.as_str()],
            )?;
        }
        if previous != next {
            insert_transition(
                &transaction,
                goal_id,
                outer_turn_id.map(GoalOuterTurnId::as_str),
                &previous,
                &next,
                reason_code,
                updated_at,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn transition_state_while_turn_in_flight(
        &self,
        goal_id: &GoalId,
        next: GoalState,
        reason_code: &str,
        outer_turn_id: &GoalOuterTurnId,
        updated_at: i64,
    ) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_goal_not_surface_owned(&transaction, goal_id, "transition")?;
        let previous = goal_state_by_id(&transaction, goal_id).map_err(|error| match error {
            GoalStoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                GoalStoreError::Invalid("goal does not exist".to_string())
            }
            error => error,
        })?;
        if matches!(previous, GoalState::Complete { .. }) && previous != next {
            return Err(GoalStoreError::Invalid(
                "complete goal cannot be downgraded by a runtime transition".to_string(),
            ));
        }
        let in_flight: bool = transaction.query_row(
            "SELECT EXISTS(
                    SELECT 1 FROM goal_turns AS turns
                    JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
                    WHERE turns.outer_turn_id = ?1 AND runs.goal_id = ?2
                      AND turns.status = 'in_flight' AND runs.in_flight = 1
                )",
            params![outer_turn_id.as_str(), goal_id.as_str()],
            |row| row.get(0),
        )?;
        if !in_flight {
            return Err(GoalStoreError::Invalid(
                "goal pause request requires the active outer turn".to_string(),
            ));
        }
        if previous != next {
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&next)?, updated_at, goal_id.as_str()],
            )?;
            insert_transition(
                &transaction,
                goal_id,
                Some(outer_turn_id.as_str()),
                &previous,
                &next,
                reason_code,
                updated_at,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn usage_event_count(&self, goal_id: &GoalId) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM goal_usage_events WHERE goal_id = ?1",
            [goal_id.as_str()],
            |row| row.get(0),
        )?)
    }

    pub fn audit_snapshot(&self, goal_id: &GoalId) -> Result<GoalAuditSnapshot, GoalStoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM goal_turns AS turns
                     JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
                     WHERE runs.goal_id = ?1),
                    (SELECT COUNT(*) FROM goal_intents AS intents
                     JOIN goal_turns AS turns ON turns.outer_turn_id = intents.outer_turn_id
                     JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
                     WHERE runs.goal_id = ?1),
                    (SELECT COUNT(*) FROM goal_usage_events WHERE goal_id = ?1),
                    (SELECT COALESCE(SUM(turns.verifier_tokens), 0)
                     FROM goal_turns AS turns
                     JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
                     WHERE runs.goal_id = ?1),
                    (SELECT COUNT(*) FROM goal_transitions WHERE goal_id = ?1),
                    (SELECT COUNT(*) FROM goal_runs
                     WHERE goal_id = ?1 AND in_flight = 1)",
                [goal_id.as_str()],
                |row| {
                    Ok(GoalAuditSnapshot {
                        outer_turns: row.get(0)?,
                        intents: row.get(1)?,
                        usage_events: row.get(2)?,
                        verifier_tokens: row.get(3)?,
                        transitions: row.get(4)?,
                        in_flight_runs: row.get(5)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn in_flight_run_count(&self) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM goal_runs WHERE in_flight = 1",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn transition_count(&self, goal_id: &GoalId) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM goal_transitions WHERE goal_id = ?1",
            [goal_id.as_str()],
            |row| row.get(0),
        )?)
    }

    pub fn goal_count(&self) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row("SELECT COUNT(*) FROM goals", [], |row| row.get(0))?)
    }

    pub fn clear_goal(&self, session_id: &str) -> Result<bool, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_session_not_surface_owned(&transaction, session_id, "clear")?;
        let goal_id = transaction
            .query_row(
                "SELECT goal_id FROM goals WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(goal_id) = goal_id.as_deref() {
            ensure_goal_not_in_flight(&transaction, goal_id, "clear")?;
        }
        let changed =
            transaction.execute("DELETE FROM goals WHERE session_id = ?1", [session_id])?;
        transaction.commit()?;
        Ok(changed == 1)
    }

    fn initialize_schema(&self) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        // WAL is a persistent database property; set it once here so ordinary
        // per-operation connections never contend on the pragma lock.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS goal_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goals (
                goal_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL UNIQUE,
                objective TEXT NOT NULL,
                objective_revision INTEGER NOT NULL,
                state TEXT NOT NULL,
                token_budget INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goal_runs (
                goal_run_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL REFERENCES goals(goal_id) ON DELETE CASCADE,
                status TEXT NOT NULL,
                origin TEXT NOT NULL,
                current_outer_turn_id TEXT,
                continuation_count INTEGER NOT NULL,
                in_flight INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                finished_at INTEGER,
                surface_operation_id TEXT,
                surface_operation_json TEXT
             );
             CREATE TABLE IF NOT EXISTS goal_turns (
                outer_turn_id TEXT PRIMARY KEY,
                goal_run_id TEXT NOT NULL REFERENCES goal_runs(goal_run_id) ON DELETE CASCADE,
                origin TEXT NOT NULL,
                provider_turn_id TEXT NOT NULL,
                status TEXT NOT NULL,
                tool_count INTEGER NOT NULL,
                model_response_count INTEGER NOT NULL,
                charged_input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                verifier_tokens INTEGER NOT NULL,
                gap_fingerprint TEXT,
                started_at INTEGER NOT NULL,
                finished_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS goal_intents (
                intent_id TEXT PRIMARY KEY,
                outer_turn_id TEXT NOT NULL REFERENCES goal_turns(outer_turn_id) ON DELETE CASCADE,
                requested_state TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                ack_code TEXT NOT NULL,
                ack_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goal_usage_events (
                usage_event_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL REFERENCES goals(goal_id) ON DELETE CASCADE,
                source TEXT NOT NULL,
                charged_input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_tokens INTEGER NOT NULL,
                verifier_tokens INTEGER NOT NULL,
                cost_micros INTEGER NOT NULL,
                elapsed_seconds INTEGER NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goal_transitions (
                transition_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL REFERENCES goals(goal_id) ON DELETE CASCADE,
                outer_turn_id TEXT,
                previous_state TEXT NOT NULL,
                next_state TEXT NOT NULL,
                reason_code TEXT NOT NULL,
                evidence_json TEXT,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goal_surface_state (
                session_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL,
                goal_revision INTEGER NOT NULL,
                objective_revision INTEGER NOT NULL,
                catalog_revision INTEGER NOT NULL,
                goal_owner_epoch INTEGER NOT NULL,
                row_present INTEGER NOT NULL,
                last_store_commit_id TEXT NOT NULL,
                last_receipt_digest BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goal_surface_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                store_commit_id TEXT NOT NULL UNIQUE,
                session_id TEXT NOT NULL,
                command_digest BLOB NOT NULL,
                receipt_digest BLOB NOT NULL,
                payload_json TEXT NOT NULL,
                acknowledged INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
             );",
        )?;
        let has_surface_operation_id = {
            let mut statement = transaction.prepare("PRAGMA table_info(goal_runs)")?;
            let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;
            for column in columns {
                if column? == "surface_operation_id" {
                    found = true;
                    break;
                }
            }
            found
        };
        if !has_surface_operation_id {
            transaction.execute(
                "ALTER TABLE goal_runs ADD COLUMN surface_operation_id TEXT",
                [],
            )?;
        }
        let has_surface_operation_json = {
            let mut statement = transaction.prepare("PRAGMA table_info(goal_runs)")?;
            let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;
            for column in columns {
                if column? == "surface_operation_json" {
                    found = true;
                    break;
                }
            }
            found
        };
        if !has_surface_operation_json {
            transaction.execute(
                "ALTER TABLE goal_runs ADD COLUMN surface_operation_json TEXT",
                [],
            )?;
        }
        transaction.execute(
            "INSERT INTO goal_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn migrate_legacy_once(&self, legacy_path: &Path) -> Result<(), GoalStoreError> {
        let connection = self.connection()?;
        let migrated = connection
            .query_row(
                "SELECT value FROM goal_meta WHERE key = ?1",
                [LEGACY_MIGRATION_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .is_some();
        drop(connection);
        if migrated {
            return Ok(());
        }
        if !legacy_path.exists() {
            let connection = self.connection()?;
            connection.execute(
                "INSERT OR REPLACE INTO goal_meta (key, value) VALUES (?1, 'absent')",
                [LEGACY_MIGRATION_KEY],
            )?;
            return Ok(());
        }

        let contents = fs::read_to_string(legacy_path).map_err(|error| {
            GoalStoreError::Migration(format!("cannot read {}: {error}", legacy_path.display()))
        })?;
        let legacy: LegacyGoalDb = serde_json::from_str(&contents).map_err(|error| {
            GoalStoreError::Migration(format!("cannot parse {}: {error}", legacy_path.display()))
        })?;
        validate_legacy_goals(&legacy)?;

        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (session_id, goal) in legacy.goals {
            let goal_id = GoalId::new();
            let state = legacy_state(&goal);
            transaction.execute(
                "INSERT INTO goals (
                    goal_id, session_id, objective, objective_revision, state,
                    token_budget, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
                params![
                    goal_id.as_str(),
                    session_id,
                    goal.objective,
                    state_json(&state)?,
                    goal.token_budget,
                    goal.created_at,
                    goal.updated_at,
                ],
            )?;
            insert_transition(
                &transaction,
                &goal_id,
                None,
                &state,
                &state,
                "legacy_migrated",
                goal.updated_at,
            )?;
            if goal.tokens_used > 0 || goal.time_used_seconds > 0 {
                transaction.execute(
                    "INSERT INTO goal_usage_events (
                        usage_event_id, goal_id, source, charged_input_tokens,
                        output_tokens, cache_tokens, verifier_tokens, cost_micros,
                        elapsed_seconds, created_at
                     ) VALUES (?1, ?2, 'legacy_migration', ?3, 0, 0, 0, 0, ?4, ?5)",
                    params![
                        format!("legacy:{}", goal.session_id),
                        goal_id.as_str(),
                        goal.tokens_used.max(0),
                        goal.time_used_seconds.max(0),
                        goal.updated_at,
                    ],
                )?;
            }
        }
        transaction.execute(
            "INSERT INTO goal_meta (key, value) VALUES (?1, 'complete')",
            [LEGACY_MIGRATION_KEY],
        )?;
        transaction.commit()?;

        let timestamp = Utc::now().timestamp();
        let stem = legacy_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("goals_1");
        let backup = legacy_path.with_file_name(format!("{stem}.migrated.{timestamp}.json"));
        fs::rename(legacy_path, &backup).map_err(|error| {
            GoalStoreError::Migration(format!(
                "database commit succeeded but cannot back up {} to {}: {error}",
                legacy_path.display(),
                backup.display()
            ))
        })?;
        Ok(())
    }

    pub(crate) fn recover_in_flight_runs(&self) -> Result<Vec<GoalRecoveryRecord>, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recoveries = {
            let mut statement = transaction.prepare(
                "SELECT runs.goal_run_id, runs.goal_id, runs.current_outer_turn_id,
                        goals.session_id
                 FROM goal_runs AS runs
                 JOIN goals ON goals.goal_id = runs.goal_id
                 LEFT JOIN goal_surface_state AS surface
                   ON surface.session_id = goals.session_id
                 WHERE runs.in_flight = 1 AND surface.session_id IS NULL",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let now = Utc::now().timestamp();
        let mut records = Vec::with_capacity(recoveries.len());
        for (run_id, goal_id, outer_turn_id, session_id) in recoveries {
            let goal_id = GoalId::parse(goal_id).map_err(GoalStoreError::Invalid)?;
            let previous = goal_state_by_id(&transaction, &goal_id)?;
            let mut recovered_state = previous.clone();
            if !matches!(previous, GoalState::Complete { .. }) {
                let next = GoalState::Paused {
                    reason: GoalPauseReason::Recovery,
                    message: format!("recovered interrupted goal run {run_id}"),
                };
                transaction.execute(
                    "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                    params![state_json(&next)?, now, goal_id.as_str()],
                )?;
                insert_transition(
                    &transaction,
                    &goal_id,
                    outer_turn_id.as_deref(),
                    &previous,
                    &next,
                    "recovered",
                    now,
                )?;
                recovered_state = next;
            }
            transaction.execute(
                "UPDATE goal_runs
                 SET status = 'recovered', in_flight = 0, finished_at = ?1
                 WHERE goal_run_id = ?2",
                params![now, run_id],
            )?;
            if let Some(ref outer_turn_id) = outer_turn_id {
                transaction.execute(
                    "UPDATE goal_turns
                     SET status = 'cancelled', finished_at = ?1
                     WHERE outer_turn_id = ?2 AND finished_at IS NULL",
                    params![now, outer_turn_id],
                )?;
            }
            records.push(GoalRecoveryRecord {
                session_id,
                goal_id,
                stale_goal_run_id: GoalRunId::parse(run_id).map_err(GoalStoreError::Invalid)?,
                outer_turn_id: outer_turn_id
                    .map(GoalOuterTurnId::parse)
                    .transpose()
                    .map_err(GoalStoreError::Invalid)?,
                recovered_state,
            });
        }
        transaction.commit()?;
        Ok(records)
    }

    fn connection(&self) -> Result<Connection, GoalStoreError> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(connection)
    }
}

fn load_stored_goal(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<StoredGoal>, GoalStoreError> {
    let row = connection
        .query_row(
            "SELECT goal_id, session_id, objective, objective_revision, state,
                    token_budget, created_at, updated_at
             FROM goals WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((
        goal_id,
        session_id,
        objective,
        revision,
        state,
        token_budget,
        created_at,
        updated_at,
    )) = row
    else {
        return Ok(None);
    };
    let goal_id = GoalId::parse(goal_id).map_err(GoalStoreError::Invalid)?;
    let current_run = load_current_run(connection, &goal_id)?;
    let last_transition = load_last_transition(connection, &goal_id)?;
    Ok(Some(StoredGoal {
        record: GoalRecord {
            goal_id: goal_id.clone(),
            session_id,
            objective,
            objective_revision: revision,
            state: parse_state(&state)?,
            token_budget,
            usage: usage_totals(connection, &goal_id)?,
            current_run,
            last_transition,
        },
        created_at,
        updated_at,
    }))
}

fn load_current_run(
    connection: &Connection,
    goal_id: &GoalId,
) -> Result<Option<GoalRunSnapshot>, GoalStoreError> {
    let row = connection
        .query_row(
            "SELECT goal_run_id, current_outer_turn_id, origin,
                    continuation_count, in_flight
             FROM goal_runs
             WHERE goal_id = ?1 AND finished_at IS NULL
             ORDER BY started_at DESC LIMIT 1",
            [goal_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            },
        )
        .optional()?;
    row.map(|(run_id, turn_id, origin, continuation_count, in_flight)| {
        Ok(GoalRunSnapshot {
            goal_run_id: GoalRunId::parse(run_id).map_err(GoalStoreError::Invalid)?,
            outer_turn_id: turn_id
                .map(GoalOuterTurnId::parse)
                .transpose()
                .map_err(GoalStoreError::Invalid)?,
            origin: parse_origin(&origin)?,
            continuation_count,
            in_flight,
        })
    })
    .transpose()
}

fn load_last_transition(
    connection: &Connection,
    goal_id: &GoalId,
) -> Result<Option<GoalTransitionSummary>, GoalStoreError> {
    let row = connection
        .query_row(
            "SELECT previous_state, next_state, reason_code
             FROM goal_transitions
             WHERE goal_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1",
            [goal_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(previous, next, reason_code)| {
        Ok(GoalTransitionSummary {
            previous_state: parse_state(&previous)?,
            next_state: parse_state(&next)?,
            reason_code,
        })
    })
    .transpose()
}

fn usage_totals(connection: &Connection, goal_id: &GoalId) -> Result<GoalUsage, GoalStoreError> {
    Ok(connection.query_row(
        "SELECT
            COALESCE(SUM(charged_input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cache_tokens), 0),
            COALESCE(SUM(verifier_tokens), 0),
            COALESCE(SUM(cost_micros), 0),
            COALESCE(SUM(elapsed_seconds), 0)
         FROM goal_usage_events WHERE goal_id = ?1",
        [goal_id.as_str()],
        |row| {
            Ok(GoalUsage {
                charged_input_tokens: row.get(0)?,
                output_tokens: row.get(1)?,
                cache_tokens: row.get(2)?,
                verifier_tokens: row.get(3)?,
                cost_micros: row.get(4)?,
                elapsed_seconds: row.get(5)?,
            })
        },
    )?)
}

fn insert_usage_event(
    transaction: &Transaction<'_>,
    event: &GoalUsageEvent,
) -> Result<(), GoalStoreError> {
    transaction.execute(
        "INSERT OR IGNORE INTO goal_usage_events (
            usage_event_id, goal_id, source, charged_input_tokens,
            output_tokens, cache_tokens, verifier_tokens, cost_micros,
            elapsed_seconds, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            event.usage_event_id,
            event.goal_id.as_str(),
            event.source,
            event.usage.charged_input_tokens.max(0),
            event.usage.output_tokens.max(0),
            event.usage.cache_tokens.max(0),
            event.usage.verifier_tokens.max(0),
            event.usage.cost_micros.max(0),
            event.usage.elapsed_seconds.max(0),
            event.created_at,
        ],
    )?;
    Ok(())
}

fn requested_state_name(state: GoalRequestedState) -> &'static str {
    match state {
        GoalRequestedState::Complete => "complete",
        GoalRequestedState::Blocked => "blocked",
    }
}

fn ack_code(ack: &GoalUpdateAck) -> &'static str {
    match ack {
        GoalUpdateAck::DeferredToTurnEnd { .. } => "deferred_to_turn_end",
        GoalUpdateAck::Rejected { .. } => "rejected",
        GoalUpdateAck::AlreadyPending { .. } => "already_pending",
        GoalUpdateAck::BlockedAgainstInactive { .. } => "blocked_against_inactive",
    }
}

fn turn_status_name(status: GoalTurnStatus) -> &'static str {
    match status {
        GoalTurnStatus::Success => "success",
        GoalTurnStatus::Failed => "failed",
        GoalTurnStatus::Cancelled => "cancelled",
        GoalTurnStatus::ApprovalRequired => "approval_required",
        GoalTurnStatus::BudgetExhausted => "budget_exhausted",
    }
}

fn closed_run_status(state: &GoalState) -> Option<&'static str> {
    match state {
        GoalState::Active => None,
        GoalState::Paused { .. } => Some("paused"),
        GoalState::Blocked { .. } => Some("blocked"),
        GoalState::BudgetLimited => Some("budget_limited"),
        GoalState::Complete { .. } => Some("complete"),
    }
}

fn core_goal_state_from_surface(state: &crate::runtime_surface::SurfaceGoalState) -> GoalState {
    use crate::runtime_surface::{
        SurfaceBlockerKind, SurfaceEvidenceKind, SurfaceGoalPauseReason, SurfaceGoalState,
    };
    let convert_evidence = |items: &[crate::runtime_surface::SurfaceEvidenceItem]| {
        items
            .iter()
            .map(|item| orca_core::goal_runtime::EvidenceItem {
                kind: match item.kind {
                    SurfaceEvidenceKind::Test => orca_core::goal_runtime::EvidenceKind::Test,
                    SurfaceEvidenceKind::File => orca_core::goal_runtime::EvidenceKind::File,
                    SurfaceEvidenceKind::Command => orca_core::goal_runtime::EvidenceKind::Command,
                    SurfaceEvidenceKind::Observation => {
                        orca_core::goal_runtime::EvidenceKind::Observation
                    }
                    SurfaceEvidenceKind::External => {
                        orca_core::goal_runtime::EvidenceKind::External
                    }
                },
                summary: item.summary.as_str().to_string(),
                target: item
                    .target
                    .as_ref()
                    .map(|target| target.as_str().to_string()),
            })
            .collect::<Vec<_>>()
    };
    match state {
        SurfaceGoalState::Active => GoalState::Active,
        SurfaceGoalState::Paused { reason, message } => GoalState::Paused {
            reason: match reason {
                SurfaceGoalPauseReason::User => GoalPauseReason::User,
                SurfaceGoalPauseReason::NoProgress => GoalPauseReason::NoProgress,
                SurfaceGoalPauseReason::Backoff => GoalPauseReason::Backoff,
                SurfaceGoalPauseReason::Infrastructure => GoalPauseReason::Infrastructure,
                SurfaceGoalPauseReason::WaitingForWorkflow => GoalPauseReason::WaitingForWorkflow,
                SurfaceGoalPauseReason::Recovery => GoalPauseReason::Recovery,
                SurfaceGoalPauseReason::UsageLimit => GoalPauseReason::UsageLimit,
            },
            message: message.as_str().to_string(),
        },
        SurfaceGoalState::Blocked { blocker } => GoalState::Blocked {
            blocker: orca_core::goal_runtime::BlockerSummary {
                kind: match blocker.kind {
                    SurfaceBlockerKind::UserDecision => {
                        orca_core::goal_runtime::BlockerKind::UserDecision
                    }
                    SurfaceBlockerKind::MissingAuthority => {
                        orca_core::goal_runtime::BlockerKind::MissingAuthority
                    }
                    SurfaceBlockerKind::ExternalState => {
                        orca_core::goal_runtime::BlockerKind::ExternalState
                    }
                    SurfaceBlockerKind::EnvironmentContradiction => {
                        orca_core::goal_runtime::BlockerKind::EnvironmentContradiction
                    }
                    SurfaceBlockerKind::UnverifiableRequirement => {
                        orca_core::goal_runtime::BlockerKind::UnverifiableRequirement
                    }
                },
                summary: blocker.summary.as_str().to_string(),
                fingerprint: blocker.fingerprint.as_str().to_string(),
                evidence: convert_evidence(&blocker.evidence),
            },
        },
        SurfaceGoalState::BudgetLimited => GoalState::BudgetLimited,
        SurfaceGoalState::Complete { evidence } => GoalState::Complete {
            evidence: convert_evidence(evidence),
        },
    }
}

fn validate_surface_mutation_context(
    context: &GoalSurfaceMutationContext,
) -> Result<(), GoalStoreError> {
    if context.store_commit_id.trim().is_empty() {
        return Err(GoalStoreError::Invalid(
            "goal surface store commit id must not be empty".to_string(),
        ));
    }
    if context.goal_owner_epoch == 0 || context.goal_owner_epoch > i64::MAX as u64 {
        return Err(GoalStoreError::Invalid(
            "goal surface owner epoch must fit a positive SQLite integer".to_string(),
        ));
    }
    Ok(())
}

fn validate_surface_goal_operation(
    operation: &OperationRecord,
    goal_id: &GoalId,
    goal_run_id: &GoalRunId,
    objective_revision: u32,
) -> Result<(), GoalStoreError> {
    let OperationKind::GoalRun {
        goal_id: operation_goal_id,
        goal_run_id: operation_goal_run_id,
        initial_objective_revision,
    } = &operation.intent.kind
    else {
        return Err(GoalStoreError::Invalid(
            "Goal surface run requires a GoalRun operation".to_string(),
        ));
    };
    if operation.phase != OperationPhase::Requested
        || operation.reservation.operation_id != operation.operation_id
        || operation_goal_id.as_str() != goal_id.as_str()
        || operation_goal_run_id.as_str() != goal_run_id.as_str()
        || initial_objective_revision.get() != objective_revision
    {
        return Err(GoalStoreError::Invalid(
            "Goal surface operation binding is inconsistent".to_string(),
        ));
    }
    Ok(())
}

fn validate_surface_owner_epoch(
    transaction: &Transaction<'_>,
    expected_epoch: u64,
) -> Result<(), GoalStoreError> {
    let current = transaction
        .query_row(
            "SELECT value FROM goal_meta WHERE key = 'surface_owner_epoch'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            GoalStoreError::Invalid("Goal surface owner lease has not been acquired".to_string())
        })?
        .parse::<u64>()
        .map_err(|_| {
            GoalStoreError::Invalid("stored Goal surface owner epoch is not a u64".to_string())
        })?;
    if current != expected_epoch {
        return Err(GoalStoreError::Invalid(format!(
            "Goal surface owner epoch is stale: expected {current}, received {expected_epoch}"
        )));
    }
    Ok(())
}

fn replay_surface_mutation(
    transaction: &Transaction<'_>,
    context: &GoalSurfaceMutationContext,
) -> Result<Option<GoalSurfaceMutationRecord>, GoalStoreError> {
    let existing = transaction
        .query_row(
            "SELECT session_id, store_commit_id, command_digest, receipt_digest, payload_json
             FROM goal_surface_outbox
             WHERE store_commit_id = ?1",
            [&context.store_commit_id],
            |row| {
                Ok(StoredSurfaceMutation {
                    session_id: row.get(0)?,
                    store_commit_id: row.get(1)?,
                    command_digest: row.get(2)?,
                    receipt_digest: row.get(3)?,
                    payload_json: row.get(4)?,
                })
            },
        )
        .optional()?;
    let Some(stored) = existing else {
        return Ok(None);
    };
    let (command_digest, mutation) = validate_stored_surface_mutation(&stored)?;
    if command_digest != context.command_digest {
        return Err(GoalStoreError::Invalid(
            "goal surface store commit id was reused for a different command".to_string(),
        ));
    }
    Ok(Some(mutation))
}

fn goal_surface_receipt(
    context: &GoalSurfaceMutationContext,
    session_id: &str,
    mutation: &GoalSurfaceMutation,
    goal_id: GoalId,
    goal_revision: u32,
    objective_revision: u32,
    catalog_revision: u32,
    row_state: GoalSurfaceRowState,
) -> Result<GoalSurfaceStoreReceipt, GoalStoreError> {
    let receipt_digest = goal_surface_receipt_digest(
        &context.store_commit_id,
        &context.command_digest,
        session_id,
        mutation,
        &goal_id,
        goal_revision,
        objective_revision,
        catalog_revision,
        context.goal_owner_epoch,
        &row_state,
    )?;
    Ok(GoalSurfaceStoreReceipt {
        store_commit_id: context.store_commit_id.clone(),
        goal_id,
        goal_revision,
        objective_revision,
        catalog_revision,
        goal_owner_epoch: context.goal_owner_epoch,
        row_state,
        receipt_digest,
    })
}

fn goal_surface_receipt_digest(
    store_commit_id: &str,
    command_digest: &[u8; 32],
    session_id: &str,
    mutation: &GoalSurfaceMutation,
    goal_id: &GoalId,
    goal_revision: u32,
    objective_revision: u32,
    catalog_revision: u32,
    goal_owner_epoch: u64,
    row_state: &GoalSurfaceRowState,
) -> Result<[u8; 32], GoalStoreError> {
    #[derive(Serialize)]
    struct DigestInput<'a> {
        store_commit_id: &'a str,
        command_digest: &'a [u8; 32],
        session_id: &'a str,
        mutation: &'a GoalSurfaceMutation,
        goal_id: &'a GoalId,
        goal_revision: u32,
        objective_revision: u32,
        catalog_revision: u32,
        goal_owner_epoch: u64,
        row_state: &'a GoalSurfaceRowState,
    }

    let digest = Sha256::digest(serde_json::to_vec(&DigestInput {
        store_commit_id,
        command_digest,
        session_id,
        mutation,
        goal_id,
        goal_revision,
        objective_revision,
        catalog_revision,
        goal_owner_epoch,
        row_state,
    })?);
    Ok(digest.into())
}

fn exact_digest(bytes: &[u8], label: &str) -> Result<[u8; 32], GoalStoreError> {
    bytes.try_into().map_err(|_| {
        GoalStoreError::Invalid(format!(
            "stored Goal surface {label} is not exactly 32 bytes"
        ))
    })
}

fn validate_stored_surface_mutation(
    stored: &StoredSurfaceMutation,
) -> Result<([u8; 32], GoalSurfaceMutationRecord), GoalStoreError> {
    let command_digest = exact_digest(&stored.command_digest, "command digest")?;
    let stored_receipt_digest = exact_digest(&stored.receipt_digest, "receipt digest")?;
    let mutation: GoalSurfaceMutationRecord = serde_json::from_str(&stored.payload_json)?;
    let receipt = &mutation.receipt;

    if mutation.session_id != stored.session_id
        || receipt.store_commit_id != stored.store_commit_id
        || receipt.receipt_digest != stored_receipt_digest
    {
        return Err(GoalStoreError::Invalid(
            "stored Goal surface outbox metadata disagrees with its payload".to_string(),
        ));
    }
    let canonical_receipt_digest = goal_surface_receipt_digest(
        &receipt.store_commit_id,
        &command_digest,
        &mutation.session_id,
        &mutation.mutation,
        &receipt.goal_id,
        receipt.goal_revision,
        receipt.objective_revision,
        receipt.catalog_revision,
        receipt.goal_owner_epoch,
        &receipt.row_state,
    )?;
    if canonical_receipt_digest != receipt.receipt_digest {
        return Err(GoalStoreError::Invalid(
            "stored Goal surface receipt failed canonical digest validation".to_string(),
        ));
    }
    let shape_matches = match (&mutation.mutation, &receipt.row_state) {
        (GoalSurfaceMutation::Created, GoalSurfaceRowState::Present(goal)) => {
            receipt.goal_revision == 1
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
        }
        (
            GoalSurfaceMutation::CreatedWithRun {
                goal_run_id,
                operation,
                origin,
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            receipt.goal_revision == 1
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && validate_surface_goal_operation(
                    operation,
                    &receipt.goal_id,
                    goal_run_id,
                    receipt.objective_revision,
                )
                .is_ok()
                && matches!(
                    goal.current_run,
                    Some(ref run)
                        if &run.goal_run_id == goal_run_id
                            && run.origin == *origin
                            && !run.in_flight
                            && run.continuation_count == 0
                            && run.outer_turn_id.is_none()
                )
        }
        (GoalSurfaceMutation::Edited { previous_revision }, GoalSurfaceRowState::Present(goal)) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
        }
        (
            GoalSurfaceMutation::Removed {
                previous_revision,
                tombstone_revision,
            },
            GoalSurfaceRowState::Removed,
        ) => {
            previous_revision.checked_add(1) == Some(*tombstone_revision)
                && receipt.goal_revision == *tombstone_revision
        }
        (
            GoalSurfaceMutation::RunStarted {
                previous_revision,
                goal_run_id,
                operation,
                origin,
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && validate_surface_goal_operation(
                    operation,
                    &receipt.goal_id,
                    goal_run_id,
                    receipt.objective_revision,
                )
                .is_ok()
                && matches!(
                    goal.current_run,
                    Some(ref run)
                        if &run.goal_run_id == goal_run_id
                            && run.origin == *origin
                            && !run.in_flight
                            && run.continuation_count == 0
                            && run.outer_turn_id.is_none()
                )
        }
        (
            GoalSurfaceMutation::OuterTurnStarted {
                previous_revision,
                identity,
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && identity.goal_id.as_str() == receipt.goal_id.as_str()
                && identity.objective_revision.get() == receipt.objective_revision
                && identity.operation_fence.generation_id.get() == 0
                && identity.predecessor_fence.is_none()
                && identity.outer_turn_count == 1
                && identity.attempt == GenerationAttempt::Initial
                && matches!(
                    goal.current_run,
                    Some(ref run)
                        if run.goal_run_id.as_str() == identity.goal_run_id.as_str()
                            && run.in_flight
                            && run.continuation_count == 1
                            && run.outer_turn_id.as_ref().is_some_and(
                                |outer_turn_id| outer_turn_id.as_str()
                                    == identity.goal_outer_turn_id.as_str()
                            )
                )
        }
        (
            GoalSurfaceMutation::OuterTurnFinished {
                previous_revision,
                identity,
                usage,
                ..
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && identity.goal_id.as_str() == receipt.goal_id.as_str()
                && identity.objective_revision.get() == receipt.objective_revision
                && usage.charged_input_tokens >= 0
                && usage.output_tokens >= 0
                && usage.cache_tokens >= 0
                && usage.verifier_tokens >= 0
                && usage.cost_micros >= 0
                && usage.elapsed_seconds >= 0
                && matches!(
                    goal.current_run,
                    Some(ref run)
                        if run.goal_run_id.as_str() == identity.goal_run_id.as_str()
                            && !run.in_flight
                            && run.outer_turn_id.is_none()
                            && run.continuation_count == identity.outer_turn_count
                )
        }
        (
            GoalSurfaceMutation::IntentRequested {
                previous_revision,
                identity,
                ..
            }
            | GoalSurfaceMutation::IntentAcknowledged {
                previous_revision,
                identity,
                ..
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && identity.goal_id.as_str() == receipt.goal_id.as_str()
                && identity.objective_revision.get() == receipt.objective_revision
                && matches!(
                    goal.current_run,
                    Some(ref run)
                        if run.goal_run_id.as_str() == identity.goal_run_id.as_str()
                            && run.in_flight
                            && run.continuation_count == identity.outer_turn_count
                            && run.outer_turn_id.as_ref().is_some_and(
                                |outer_turn_id| outer_turn_id.as_str()
                                    == identity.goal_outer_turn_id.as_str()
                            )
                )
        }
        (
            GoalSurfaceMutation::VerificationCompleted {
                previous_revision,
                identity,
                ..
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && identity.goal_id.as_str() == receipt.goal_id.as_str()
                && identity.objective_revision.get() == receipt.objective_revision
                && matches!(
                    goal.current_run,
                    Some(ref run)
                        if run.goal_run_id.as_str() == identity.goal_run_id.as_str()
                            && !run.in_flight
                            && run.outer_turn_id.is_none()
                            && run.continuation_count == identity.outer_turn_count
                )
        }
        (
            GoalSurfaceMutation::Paused {
                previous_revision,
                goal_run_id,
                operation_id: _,
                outer_turn_id,
                message,
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && matches!(
                    (&goal.state, &goal.current_run),
                    (
                        GoalState::Paused {
                            reason: GoalPauseReason::User,
                            message: stored_message,
                        },
                        Some(run),
                    ) if stored_message == message
                        && &run.goal_run_id == goal_run_id
                        && run.outer_turn_id.as_ref() == outer_turn_id.as_ref()
                )
        }
        (
            GoalSurfaceMutation::PausedQuiescent {
                previous_revision,
                message,
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && goal.current_run.is_none()
                && matches!(
                    &goal.state,
                    GoalState::Paused {
                        reason: GoalPauseReason::User,
                        message: stored_message,
                    } if stored_message == message
                )
        }
        (
            GoalSurfaceMutation::ContinuationStopped {
                previous_revision,
                predecessor,
                decision,
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && goal.current_run.is_none()
                && predecessor.goal_id.as_str() == receipt.goal_id.as_str()
                && matches!(
                    decision.as_ref(),
                    crate::runtime_surface::GoalContinuationDecision::Stopped {
                        outer_turn_count,
                        goal_state,
                        ..
                    } if *outer_turn_count == predecessor.outer_turn_count
                        && !matches!(
                            goal_state,
                            crate::runtime_surface::SurfaceGoalState::Active
                        )
                        && goal.state == core_goal_state_from_surface(goal_state)
                )
        }
        (
            GoalSurfaceMutation::ContinuationAdmitted {
                previous_revision,
                predecessor,
                decision,
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && predecessor.goal_id.as_str() == receipt.goal_id.as_str()
                && matches!(
                    (decision.as_ref(), goal.current_run.as_ref()),
                    (
                        crate::runtime_surface::GoalContinuationDecision::Admitted {
                            successor,
                            ..
                        },
                        Some(run),
                    ) if successor.goal_id == predecessor.goal_id
                        && successor.goal_run_id == predecessor.goal_run_id
                        && successor.operation_fence.operation_id
                            == predecessor.operation_fence.operation_id
                        && successor.predecessor_fence.as_ref()
                            == Some(&predecessor.operation_fence)
                        && predecessor.outer_turn_count.checked_add(1)
                            == Some(successor.outer_turn_count)
                        && run.goal_run_id.as_str() == successor.goal_run_id.as_str()
                        && run.in_flight
                        && run.outer_turn_id.as_ref().is_some_and(|outer_turn_id| {
                            outer_turn_id.as_str() == successor.goal_outer_turn_id.as_str()
                        })
                        && run.continuation_count == successor.outer_turn_count
                )
        }
        (
            GoalSurfaceMutation::Recovered {
                previous_revision,
                stale_goal_run_id,
                operation,
                origin,
                stale_identity,
                stale_run_settled: _,
                recovery_message,
            },
            GoalSurfaceRowState::Present(goal),
        ) => {
            previous_revision.checked_add(1) == Some(receipt.goal_revision)
                && goal.session_id == mutation.session_id
                && goal.goal_id == receipt.goal_id
                && goal.objective_revision == receipt.objective_revision
                && goal.current_run.is_none()
                && *origin != GoalTurnOrigin::Continuation
                && matches!(
                    &goal.state,
                    GoalState::Paused {
                        reason: GoalPauseReason::Recovery,
                        message,
                    } if message == recovery_message
                )
                && validate_surface_goal_operation(
                    operation,
                    &receipt.goal_id,
                    stale_goal_run_id,
                    receipt.objective_revision,
                )
                .is_ok()
                && stale_identity.as_ref().is_none_or(|identity| {
                    identity.goal_id.as_str() == receipt.goal_id.as_str()
                        && identity.goal_run_id.as_str() == stale_goal_run_id.as_str()
                        && identity.operation_fence.operation_id == operation.operation_id
                        && identity.objective_revision.get() == receipt.objective_revision
                })
        }
        _ => false,
    };
    if !shape_matches
        || receipt.goal_revision == 0
        || receipt.objective_revision == 0
        || receipt.catalog_revision == 0
        || receipt.goal_owner_epoch == 0
    {
        return Err(GoalStoreError::Invalid(
            "stored Goal surface mutation payload is structurally inconsistent".to_string(),
        ));
    }
    Ok((command_digest, mutation))
}

fn persist_surface_mutation(
    transaction: &Transaction<'_>,
    context: &GoalSurfaceMutationContext,
    output: &GoalSurfaceMutationRecord,
    created_at: i64,
) -> Result<(), GoalStoreError> {
    transaction.execute(
        "INSERT INTO goal_surface_outbox (
            store_commit_id, session_id, command_digest, receipt_digest,
            payload_json, acknowledged, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
        params![
            context.store_commit_id,
            output.session_id,
            context.command_digest.as_slice(),
            output.receipt.receipt_digest.as_slice(),
            serde_json::to_string(output)?,
            created_at,
        ],
    )?;
    Ok(())
}

fn supersede_goal_continuation_outbox(
    transaction: &Transaction<'_>,
    session_id: &str,
    predecessor: &SurfaceGoalGenerationIdentity,
    interrupted_store_commit_id: &str,
) -> Result<(), GoalStoreError> {
    let pending = {
        let mut statement = transaction.prepare(
            "SELECT session_id, store_commit_id, command_digest, receipt_digest, payload_json
             FROM goal_surface_outbox
             WHERE session_id = ?1 AND acknowledged = 0
             ORDER BY sequence ASC",
        )?;
        statement
            .query_map([session_id], |row| {
                Ok(StoredSurfaceMutation {
                    session_id: row.get(0)?,
                    store_commit_id: row.get(1)?,
                    command_digest: row.get(2)?,
                    receipt_digest: row.get(3)?,
                    payload_json: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut superseded_interrupted = false;
    for stored in pending {
        let (_, mutation) = validate_stored_surface_mutation(&stored)?;
        let same_predecessor = match &mutation.mutation {
            GoalSurfaceMutation::OuterTurnFinished { identity, .. }
            | GoalSurfaceMutation::VerificationCompleted { identity, .. } => {
                identity.as_ref() == predecessor
            }
            GoalSurfaceMutation::ContinuationStopped {
                predecessor: candidate,
                ..
            }
            | GoalSurfaceMutation::ContinuationAdmitted {
                predecessor: candidate,
                ..
            } => candidate.as_ref() == predecessor,
            _ => false,
        };
        if !same_predecessor {
            continue;
        }
        superseded_interrupted |= stored.store_commit_id == interrupted_store_commit_id;
        let changed = transaction.execute(
            "UPDATE goal_surface_outbox
             SET acknowledged = 1
             WHERE store_commit_id = ?1 AND receipt_digest = ?2 AND acknowledged = 0",
            params![stored.store_commit_id, stored.receipt_digest],
        )?;
        if changed != 1 {
            return Err(GoalStoreError::Invalid(
                "Goal continuation supersession lost an exact outbox row".to_string(),
            ));
        }
    }
    if !superseded_interrupted {
        return Err(GoalStoreError::Invalid(
            "Goal continuation recovery did not supersede its interrupted receipt".to_string(),
        ));
    }
    Ok(())
}

fn persist_surface_state(
    transaction: &Transaction<'_>,
    output: &GoalSurfaceMutationRecord,
) -> Result<(), GoalStoreError> {
    let row_present = i64::from(matches!(
        output.receipt.row_state,
        GoalSurfaceRowState::Present(_)
    ));
    transaction.execute(
        "INSERT INTO goal_surface_state (
            session_id, goal_id, goal_revision, objective_revision,
            catalog_revision, goal_owner_epoch, row_present,
            last_store_commit_id, last_receipt_digest
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(session_id) DO UPDATE SET
            goal_id = excluded.goal_id,
            goal_revision = excluded.goal_revision,
            objective_revision = excluded.objective_revision,
            catalog_revision = excluded.catalog_revision,
            goal_owner_epoch = excluded.goal_owner_epoch,
            row_present = excluded.row_present,
            last_store_commit_id = excluded.last_store_commit_id,
            last_receipt_digest = excluded.last_receipt_digest",
        params![
            output.session_id,
            output.receipt.goal_id.as_str(),
            output.receipt.goal_revision,
            output.receipt.objective_revision,
            output.receipt.catalog_revision,
            i64::try_from(output.receipt.goal_owner_epoch).map_err(|_| {
                GoalStoreError::Invalid("goal surface owner epoch overflowed SQLite".to_string())
            })?,
            row_present,
            output.receipt.store_commit_id,
            output.receipt.receipt_digest.as_slice(),
        ],
    )?;
    Ok(())
}

fn load_goal_surface_state(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<StoredGoalSurfaceState>, GoalStoreError> {
    let stored = connection
        .query_row(
            "SELECT goal_id, goal_revision, objective_revision, catalog_revision,
                    goal_owner_epoch, row_present, last_receipt_digest
             FROM goal_surface_state WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((
        goal_id,
        goal_revision,
        objective_revision,
        catalog_revision,
        goal_owner_epoch,
        row_present,
        last_receipt_digest,
    )) = stored
    else {
        return Ok(None);
    };
    let positive_u32 = |value: i64, name: &str| {
        u32::try_from(value)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                GoalStoreError::Invalid(format!("stored goal surface {name} is not a positive u32"))
            })
    };
    let goal_owner_epoch = u64::try_from(goal_owner_epoch)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            GoalStoreError::Invalid("stored goal surface owner epoch is not positive".to_string())
        })?;
    Ok(Some(StoredGoalSurfaceState {
        goal_id: GoalId::parse(goal_id).map_err(GoalStoreError::Invalid)?,
        goal_revision: positive_u32(goal_revision, "revision")?,
        objective_revision: positive_u32(objective_revision, "objective revision")?,
        catalog_revision: positive_u32(catalog_revision, "catalog revision")?,
        goal_owner_epoch,
        row_present: row_present == 1,
        last_receipt_digest: exact_digest(&last_receipt_digest, "receipt digest")?,
    }))
}

fn goal_state_by_id(
    connection: &Connection,
    goal_id: &GoalId,
) -> Result<GoalState, GoalStoreError> {
    let state: String = connection.query_row(
        "SELECT state FROM goals WHERE goal_id = ?1",
        [goal_id.as_str()],
        |row| row.get(0),
    )?;
    parse_state(&state)
}

fn ensure_goal_not_in_flight(
    transaction: &Transaction<'_>,
    goal_id: &str,
    action: &str,
) -> Result<(), GoalStoreError> {
    let in_flight: bool = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM goal_runs WHERE goal_id = ?1 AND in_flight = 1
        )",
        [goal_id],
        |row| row.get(0),
    )?;
    if in_flight {
        return Err(GoalStoreError::Invalid(format!(
            "cannot {action} goal while an outer turn is in flight"
        )));
    }
    Ok(())
}

fn ensure_session_not_surface_owned(
    transaction: &Transaction<'_>,
    session_id: &str,
    action: &str,
) -> Result<(), GoalStoreError> {
    let surface_owned: bool = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM goal_surface_state WHERE session_id = ?1
        )",
        [session_id],
        |row| row.get(0),
    )?;
    if surface_owned {
        return Err(GoalStoreError::Invalid(format!(
            "cannot {action} a surface-owned Goal through the legacy store path"
        )));
    }
    Ok(())
}

fn ensure_goal_not_surface_owned(
    transaction: &Transaction<'_>,
    goal_id: &GoalId,
    action: &str,
) -> Result<(), GoalStoreError> {
    let surface_owned: bool = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM goals
            JOIN goal_surface_state AS surface
              ON surface.session_id = goals.session_id
            WHERE goals.goal_id = ?1
        )",
        [goal_id.as_str()],
        |row| row.get(0),
    )?;
    if surface_owned {
        return Err(GoalStoreError::Invalid(format!(
            "cannot {action} a surface-owned Goal through the legacy store path"
        )));
    }
    Ok(())
}

fn ensure_outer_turn_not_surface_owned(
    transaction: &Transaction<'_>,
    outer_turn_id: &GoalOuterTurnId,
    action: &str,
) -> Result<(), GoalStoreError> {
    let surface_owned: bool = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM goal_turns AS turns
            JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
            JOIN goals ON goals.goal_id = runs.goal_id
            JOIN goal_surface_state AS surface
              ON surface.session_id = goals.session_id
            WHERE turns.outer_turn_id = ?1
        )",
        [outer_turn_id.as_str()],
        |row| row.get(0),
    )?;
    if surface_owned {
        return Err(GoalStoreError::Invalid(format!(
            "cannot {action} a surface-owned Goal through the legacy store path"
        )));
    }
    Ok(())
}

fn ensure_outer_turn_surface_owned(
    transaction: &Transaction<'_>,
    outer_turn_id: &GoalOuterTurnId,
) -> Result<(), GoalStoreError> {
    let surface_owned: bool = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM goal_turns AS turns
            JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
            JOIN goals ON goals.goal_id = runs.goal_id
            JOIN goal_surface_state AS surface
              ON surface.session_id = goals.session_id
            WHERE turns.outer_turn_id = ?1
        )",
        [outer_turn_id.as_str()],
        |row| row.get(0),
    )?;
    if !surface_owned {
        return Err(GoalStoreError::Invalid(
            "surface Goal intent does not name a surface-owned outer turn".to_string(),
        ));
    }
    Ok(())
}

fn insert_transition(
    transaction: &Transaction<'_>,
    goal_id: &GoalId,
    outer_turn_id: Option<&str>,
    previous: &GoalState,
    next: &GoalState,
    reason_code: &str,
    created_at: i64,
) -> Result<(), GoalStoreError> {
    transaction.execute(
        "INSERT INTO goal_transitions (
            transition_id, goal_id, outer_turn_id, previous_state,
            next_state, reason_code, evidence_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
        params![
            format!("transition_{}", uuid::Uuid::now_v7()),
            goal_id.as_str(),
            outer_turn_id,
            state_json(previous)?,
            state_json(next)?,
            reason_code,
            created_at,
        ],
    )?;
    Ok(())
}

fn state_json(state: &GoalState) -> Result<String, GoalStoreError> {
    Ok(serde_json::to_string(state)?)
}

fn parse_state(value: &str) -> Result<GoalState, GoalStoreError> {
    Ok(serde_json::from_str(value)?)
}

fn origin_name(origin: GoalTurnOrigin) -> &'static str {
    match origin {
        GoalTurnOrigin::User => "user",
        GoalTurnOrigin::Resume => "resume",
        GoalTurnOrigin::Continuation => "continuation",
        GoalTurnOrigin::WorkflowNotification => "workflow_notification",
    }
}

fn parse_origin(value: &str) -> Result<GoalTurnOrigin, GoalStoreError> {
    match value {
        "user" => Ok(GoalTurnOrigin::User),
        "resume" => Ok(GoalTurnOrigin::Resume),
        "continuation" => Ok(GoalTurnOrigin::Continuation),
        "workflow_notification" => Ok(GoalTurnOrigin::WorkflowNotification),
        _ => Err(GoalStoreError::Invalid(format!(
            "unknown goal turn origin '{value}'"
        ))),
    }
}

fn validate_legacy_goals(legacy: &LegacyGoalDb) -> Result<(), GoalStoreError> {
    let mut sessions = HashSet::new();
    for (key, goal) in &legacy.goals {
        if key != &goal.session_id {
            return Err(GoalStoreError::Migration(format!(
                "goal key '{key}' does not match session id '{}'",
                goal.session_id
            )));
        }
        if !sessions.insert(goal.session_id.as_str()) {
            return Err(GoalStoreError::Migration(format!(
                "duplicate legacy session id '{}'",
                goal.session_id
            )));
        }
        validate_thread_goal_objective(&goal.objective).map_err(GoalStoreError::Migration)?;
    }
    Ok(())
}

fn legacy_state(goal: &ThreadGoal) -> GoalState {
    match goal.status {
        ThreadGoalStatus::Active => GoalState::Active,
        ThreadGoalStatus::Paused => GoalState::Paused {
            reason: GoalPauseReason::User,
            message: "migrated legacy paused goal".to_string(),
        },
        ThreadGoalStatus::Blocked => GoalState::Blocked {
            blocker: BlockerSummary {
                kind: BlockerKind::UnverifiableRequirement,
                summary: "migrated legacy blocked goal without structured evidence".to_string(),
                fingerprint: format!("legacy-blocked:{}", goal.session_id),
                evidence: Vec::new(),
            },
        },
        ThreadGoalStatus::Stalled => GoalState::Paused {
            reason: GoalPauseReason::NoProgress,
            message: "migrated legacy stalled goal".to_string(),
        },
        ThreadGoalStatus::UsageLimited => GoalState::Paused {
            reason: GoalPauseReason::UsageLimit,
            message: "migrated legacy usage-limited goal".to_string(),
        },
        ThreadGoalStatus::BudgetLimited => GoalState::BudgetLimited,
        ThreadGoalStatus::Complete => GoalState::Complete {
            evidence: Vec::new(),
        },
    }
}

fn orca_home() -> PathBuf {
    if let Ok(value) = std::env::var("ORCA_HOME")
        && !value.trim().is_empty()
    {
        return PathBuf::from(value);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".orca")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use orca_core::goal_runtime::{
        EvidenceItem, GoalPauseReason, GoalRequestedState, GoalRunId, GoalState, GoalTurnOrigin,
        GoalTurnStatus, GoalUpdateAck, GoalUpdateIntent, GoalUsage, IntentId,
    };
    use orca_core::goal_types::{ThreadGoal, ThreadGoalStatus};
    use tempfile::tempdir;

    use super::*;

    fn create_goal(store: &GoalStore, session_id: &str) -> GoalRecord {
        store
            .create_goal(CreateGoalInput {
                session_id: session_id.to_string(),
                objective: "ship runtime-owned goals".to_string(),
                token_budget: Some(100_000),
                now: 100,
            })
            .unwrap()
    }

    #[test]
    fn concurrent_opens_initialize_one_complete_schema() {
        let directory = tempdir().expect("temp goal directory");
        let path = directory.path().join("goals.sqlite3");
        let workers = 8;
        let barrier = Arc::new(std::sync::Barrier::new(workers));
        let mut handles = Vec::new();
        for _ in 0..workers {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                GoalStore::open(&path).expect("concurrent goal store open")
            }));
        }
        for handle in handles {
            let opened = handle.join().expect("concurrent goal store open worker");
            assert_eq!(
                opened.schema_version().expect("schema version"),
                4,
                "every concurrent open sees the complete schema"
            );
        }
        let store = GoalStore::open(&path).expect("final goal store open");
        assert_eq!(store.schema_version().expect("schema version"), 4);
        store
            .create_goal(CreateGoalInput {
                session_id: "concurrent-open".to_string(),
                objective: "verify schema after concurrent opens".to_string(),
                token_budget: Some(10),
                now: 1,
            })
            .expect("create goal after concurrent opens");
    }

    #[test]
    fn open_waits_for_concurrent_schema_initialization_instead_of_failing() {
        let directory = tempdir().expect("temp goal directory");
        let path = directory.path().join("goals.sqlite3");
        let workers = 32;
        let barrier = Arc::new(std::sync::Barrier::new(workers));
        let mut handles = Vec::new();
        for worker in 0..workers {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                let opened = GoalStore::open(&path);
                if let Ok(store) = opened.as_ref() {
                    let _ = store.schema_version();
                    let _ = store.create_goal(CreateGoalInput {
                        session_id: format!("concurrent-worker-{worker}"),
                        objective: "verify concurrent open writes".to_string(),
                        token_budget: Some(10),
                        now: worker as i64,
                    });
                }
                opened
            }));
        }
        let results = handles
            .into_iter()
            .map(|handle| handle.join().expect("open worker"))
            .collect::<Vec<_>>();
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            workers,
            "all concurrent opens must succeed once initialization is fenced"
        );
    }

    fn surface_context(
        store_commit_id: &str,
        command_digest: [u8; 32],
        goal_owner_epoch: u64,
    ) -> GoalSurfaceMutationContext {
        GoalSurfaceMutationContext {
            store_commit_id: store_commit_id.to_string(),
            command_digest,
            goal_owner_epoch,
        }
    }

    fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
        let mut bytes = [seed; 16];
        bytes[6] = 0x70 | (seed & 0x0f);
        bytes[8] = 0x80 | (seed & 0x3f);
        bytes
    }

    fn requested_goal_operation(
        goal_id: &GoalId,
        goal_run_id: &GoalRunId,
        operation_id: crate::runtime_surface::SurfaceOperationId,
        objective_revision: u32,
    ) -> crate::runtime_surface::OperationRecord {
        use crate::runtime_surface as surface;

        let settings_revision = surface::SettingsRevision::try_new(1).unwrap();
        let policy_epoch = surface::PolicyEpoch::try_new(1).unwrap();
        let replayability = surface::Replayability::NonReplayable {
            reason: surface::NonReplayableReason::Missing,
            live_capsule: surface::LiveOperationCapsule::Available {
                incarnation: surface::SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(42))
                    .unwrap(),
            },
        };
        surface::OperationRecord {
            operation_id: operation_id.clone(),
            request_id: surface::SurfaceRequestId::try_from_bytes(uuid_v7_bytes(43)).unwrap(),
            intent: surface::OperationIntent {
                origin: surface::OperationOrigin::TuiUser,
                kind: surface::OperationKind::GoalRun {
                    goal_id: surface::SurfaceGoalId::try_new(goal_id.to_string()).unwrap(),
                    goal_run_id: surface::SurfaceGoalRunId::try_new(goal_run_id.to_string())
                        .unwrap(),
                    initial_objective_revision: surface::GoalObjectiveRevision::new(
                        objective_revision,
                    ),
                },
                initial_replayability: replayability,
                busy_disposition: surface::BusyDisposition::Queue,
                interrupt_settlement: surface::InterruptSettlement::SuspendUntilExplicitControl,
                legacy_visibility: surface::LegacyVisibility::PublishAfterAdmitted,
                settings_revision,
                policy_epoch,
                required_capabilities: Default::default(),
                capability_fingerprint: surface::Sha256Digest::new([44; 32]),
                settings_receipt: surface::OperationSettingsPreparationReceipt::Current {
                    settings_revision,
                    policy_epoch,
                },
            },
            phase: surface::OperationPhase::Requested,
            reservation: surface::ReservationLease::new(
                surface::SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(45)).unwrap(),
                operation_id,
                surface::SequenceNumber::new(1),
                surface::HostIncarnation::try_from_bytes(uuid_v7_bytes(46)).unwrap(),
                surface::MonotonicInstant {
                    clock_id: surface::HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(47))
                        .unwrap(),
                    tick: surface::MonotonicTick::new(0),
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
        }
    }

    #[test]
    fn sqlite_store_creates_and_projects_goal_state() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-1");

        let record = store.get_by_session("session-1").unwrap().unwrap();
        let projection = store.project_thread_goal("session-1").unwrap().unwrap();

        assert_eq!(record.goal_id, goal.goal_id);
        assert_eq!(record.state, GoalState::Active);
        assert_eq!(projection.status, ThreadGoalStatus::Active);
        assert_eq!(projection.tokens_used, 0);
        assert_eq!(store.schema_version().unwrap(), 4);
    }

    #[test]
    fn surface_create_is_atomic_replayable_and_acknowledged_by_exact_receipt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals.sqlite3");
        let store = GoalStore::open(&path).unwrap();
        let input = CreateGoalInput {
            session_id: "surface-create".to_string(),
            objective: "ship a recoverable Goal surface".to_string(),
            token_budget: Some(42_000),
            now: 100,
        };
        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let context = surface_context("019f8b4d-7d73-7b52-8f44-2cfeac060001", [1; 32], owner_epoch);

        let first = store
            .create_goal_for_surface(input.clone(), context.clone())
            .unwrap();
        let replay = store
            .create_goal_for_surface(input, context.clone())
            .unwrap();

        assert_eq!(replay, first);
        assert_eq!(store.goal_count().unwrap(), 1);
        assert_eq!(first.receipt.goal_revision, 1);
        assert_eq!(first.receipt.catalog_revision, 1);
        assert_eq!(first.receipt.goal_owner_epoch, owner_epoch);
        assert!(matches!(first.mutation, GoalSurfaceMutation::Created));
        assert!(matches!(
            first.receipt.row_state,
            GoalSurfaceRowState::Present(ref goal)
                if goal.session_id == "surface-create"
                    && goal.objective == "ship a recoverable Goal surface"
        ));

        let conflict = store
            .create_goal_for_surface(
                CreateGoalInput {
                    session_id: "surface-create".to_string(),
                    objective: "different command".to_string(),
                    token_budget: Some(42_000),
                    now: 100,
                },
                GoalSurfaceMutationContext {
                    command_digest: [2; 32],
                    ..context.clone()
                },
            )
            .unwrap_err();
        assert!(conflict.to_string().contains("different command"));

        drop(store);
        let reopened = GoalStore::open(&path).unwrap();
        assert_eq!(
            reopened
                .pending_surface_mutations("surface-create")
                .unwrap(),
            vec![first.clone()]
        );
        assert!(
            !reopened
                .acknowledge_surface_mutation(&context.store_commit_id, &[9; 32], owner_epoch,)
                .unwrap()
        );
        assert_eq!(
            reopened
                .pending_surface_mutations("surface-create")
                .unwrap(),
            vec![first.clone()]
        );
        assert!(
            reopened
                .acknowledge_surface_mutation(
                    &context.store_commit_id,
                    &first.receipt.receipt_digest,
                    owner_epoch,
                )
                .unwrap()
        );
        assert!(
            reopened
                .pending_surface_mutations("surface-create")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened
                .create_goal_for_surface(
                    CreateGoalInput {
                        session_id: "surface-create".to_string(),
                        objective: "ship a recoverable Goal surface".to_string(),
                        token_budget: Some(42_000),
                        now: 100,
                    },
                    context,
                )
                .unwrap(),
            first
        );
    }

    #[test]
    fn surface_run_preparation_atomically_binds_the_exact_operation_and_replays_after_restart() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals.sqlite3");
        let store = GoalStore::open(&path).unwrap();
        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let created = store
            .create_goal_for_surface(
                CreateGoalInput {
                    session_id: "surface-run-preparation".to_string(),
                    objective: "run through one runtime-owned operation".to_string(),
                    token_budget: Some(20_000),
                    now: 100,
                },
                surface_context(
                    "019f8b4d-7d73-7b52-8f44-2cfeac060110",
                    [40; 32],
                    owner_epoch,
                ),
            )
            .unwrap();
        assert!(
            store
                .acknowledge_surface_mutation(
                    &created.receipt.store_commit_id,
                    &created.receipt.receipt_digest,
                    owner_epoch,
                )
                .unwrap()
        );
        let mut operation_bytes = [41; 16];
        operation_bytes[6] = 0x79;
        operation_bytes[8] = 0xa9;
        let operation_id =
            crate::runtime_surface::SurfaceOperationId::try_from_bytes(operation_bytes).unwrap();
        let goal_run_id = GoalRunId::new();
        let operation = requested_goal_operation(
            &created.receipt.goal_id,
            &goal_run_id,
            operation_id.clone(),
            created.receipt.objective_revision,
        );
        let context = surface_context(
            "019f8b4d-7d73-7b52-8f44-2cfeac060111",
            [41; 32],
            owner_epoch,
        );
        let expected_receipt_digest = created.receipt.receipt_digest;

        assert!(matches!(
            store.prepare_goal_run_for_surface(
                PrepareGoalRunForSurfaceInput {
                    session_id: "surface-run-preparation".to_string(),
                    expected_goal_id: created.receipt.goal_id.clone(),
                    expected_goal_revision: created.receipt.goal_revision,
                    expected_receipt_digest: [42; 32],
                    goal_run_id: goal_run_id.clone(),
                    operation: Box::new(operation.clone()),
                    origin: GoalTurnOrigin::User,
                    started_at: 101,
                },
                surface_context(
                    "019f8b4d-7d73-7b52-8f44-2cfeac060112",
                    [42; 32],
                    owner_epoch,
                ),
            ),
            Err(GoalStoreError::Invalid(message)) if message.contains("fence is stale")
        ));

        let prepared = store
            .prepare_goal_run_for_surface(
                PrepareGoalRunForSurfaceInput {
                    session_id: "surface-run-preparation".to_string(),
                    expected_goal_id: created.receipt.goal_id.clone(),
                    expected_goal_revision: created.receipt.goal_revision,
                    expected_receipt_digest,
                    goal_run_id: goal_run_id.clone(),
                    operation: Box::new(operation.clone()),
                    origin: GoalTurnOrigin::User,
                    started_at: 101,
                },
                context.clone(),
            )
            .unwrap();

        assert_eq!(prepared.receipt.goal_revision, 2);
        assert!(matches!(
            prepared.mutation,
            GoalSurfaceMutation::RunStarted {
                goal_run_id: ref recorded_run_id,
                operation: ref recorded_operation,
                origin: GoalTurnOrigin::User,
                ..
            } if recorded_run_id == &goal_run_id
                && recorded_operation.operation_id == operation_id
        ));
        assert!(matches!(
            prepared.receipt.row_state,
            GoalSurfaceRowState::Present(ref goal)
                if matches!(
                    goal.current_run,
                    Some(ref run)
                        if run.goal_run_id == goal_run_id
                            && run.origin == GoalTurnOrigin::User
                            && !run.in_flight
                            && run.continuation_count == 0
                )
        ));
        let stored_operation_id: String = store
            .connection()
            .unwrap()
            .query_row(
                "SELECT surface_operation_id FROM goal_runs WHERE goal_run_id = ?1",
                [goal_run_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored_operation_id,
            uuid::Uuid::from_bytes(*operation_id.as_bytes())
                .hyphenated()
                .to_string()
        );
        drop(store);

        let reopened = GoalStore::open(&path).unwrap();
        assert_eq!(
            reopened
                .prepare_goal_run_for_surface(
                    PrepareGoalRunForSurfaceInput {
                        session_id: "surface-run-preparation".to_string(),
                        expected_goal_id: created.receipt.goal_id,
                        expected_goal_revision: created.receipt.goal_revision,
                        expected_receipt_digest,
                        goal_run_id,
                        operation: Box::new(operation),
                        origin: GoalTurnOrigin::User,
                        started_at: 101,
                    },
                    context,
                )
                .unwrap(),
            prepared,
            "the run preparation commit identity must replay exactly after restart"
        );
        assert_eq!(
            reopened
                .pending_surface_mutations("surface-run-preparation")
                .unwrap(),
            vec![prepared]
        );
    }

    #[test]
    fn stale_surface_owner_epoch_cannot_mutate_or_append_an_outbox_record() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let first_owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let created = store
            .create_goal_for_surface(
                CreateGoalInput {
                    session_id: "stale-surface-owner".to_string(),
                    objective: "retain the first committed objective".to_string(),
                    token_budget: None,
                    now: 100,
                },
                surface_context(
                    "019f8b4d-7d73-7b52-8f44-2cfeac060020",
                    [20; 32],
                    first_owner_epoch,
                ),
            )
            .unwrap();
        let next_owner_epoch = store.claim_surface_owner_epoch().unwrap();
        assert!(next_owner_epoch > first_owner_epoch);
        let stale_ack_error = store
            .acknowledge_surface_mutation(
                &created.receipt.store_commit_id,
                &created.receipt.receipt_digest,
                first_owner_epoch,
            )
            .unwrap_err();

        let error = store
            .edit_goal_for_surface(
                "stale-surface-owner",
                &created.receipt.goal_id,
                created.receipt.goal_revision,
                "must not commit from the stale owner",
                GoalSurfaceTokenBudgetUpdate::Keep,
                101,
                surface_context(
                    "019f8b4d-7d73-7b52-8f44-2cfeac060021",
                    [21; 32],
                    first_owner_epoch,
                ),
            )
            .unwrap_err();

        assert!(stale_ack_error.to_string().contains("owner epoch is stale"));
        assert!(error.to_string().contains("owner epoch is stale"));
        assert_eq!(
            store
                .get_by_session("stale-surface-owner")
                .unwrap()
                .unwrap()
                .objective,
            "retain the first committed objective"
        );
        assert_eq!(
            store
                .pending_surface_mutations("stale-surface-owner")
                .unwrap(),
            vec![created]
        );
    }

    #[test]
    fn legacy_mutation_paths_cannot_bypass_a_surface_owned_goal() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let created = store
            .create_goal_for_surface(
                CreateGoalInput {
                    session_id: "surface-owned-legacy-guard".to_string(),
                    objective: "keep one durable Goal owner".to_string(),
                    token_budget: Some(1_000),
                    now: 100,
                },
                surface_context(
                    "019f8b4d-7d73-7b52-8f44-2cfeac060023",
                    [23; 32],
                    owner_epoch,
                ),
            )
            .unwrap();

        let edit_error = store
            .edit_goal(
                "surface-owned-legacy-guard",
                "legacy edit must not commit",
                None,
                101,
            )
            .unwrap_err();
        let clear_error = store.clear_goal("surface-owned-legacy-guard").unwrap_err();
        let begin_run_error = store
            .begin_run(BeginGoalRunInput {
                goal_id: created.receipt.goal_id.clone(),
                goal_run_id: GoalRunId::new(),
                origin: GoalTurnOrigin::User,
                started_at: 102,
            })
            .unwrap_err();
        let usage_error = store
            .record_usage_once(GoalUsageEvent {
                usage_event_id: "legacy-usage-must-not-commit".to_string(),
                goal_id: created.receipt.goal_id.clone(),
                source: "model".to_string(),
                usage: GoalUsage {
                    charged_input_tokens: 10,
                    output_tokens: 5,
                    ..GoalUsage::default()
                },
                created_at: 103,
            })
            .unwrap_err();
        let transition_error = store
            .transition_state(
                &created.receipt.goal_id,
                GoalState::Paused {
                    reason: GoalPauseReason::User,
                    message: "legacy transition must not commit".to_string(),
                },
                "legacy_pause",
                None,
                104,
            )
            .unwrap_err();

        for error in [
            edit_error,
            clear_error,
            begin_run_error,
            usage_error,
            transition_error,
        ] {
            assert!(error.to_string().contains("legacy store path"));
        }
        let stored = store
            .get_by_session("surface-owned-legacy-guard")
            .unwrap()
            .unwrap();
        assert_eq!(stored.objective, "keep one durable Goal owner");
        assert_eq!(stored.state, GoalState::Active);
        assert_eq!(stored.usage, GoalUsage::default());
        assert!(stored.current_run.is_none());
        assert_eq!(
            store
                .pending_surface_mutations("surface-owned-legacy-guard")
                .unwrap(),
            vec![created]
        );
    }

    #[test]
    fn corrupted_surface_outbox_payload_fails_replay_scan_and_acknowledgement_closed() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let input = CreateGoalInput {
            session_id: "corrupted-surface-outbox".to_string(),
            objective: "canonical objective".to_string(),
            token_budget: None,
            now: 100,
        };
        let context = surface_context(
            "019f8b4d-7d73-7b52-8f44-2cfeac060022",
            [22; 32],
            owner_epoch,
        );
        let created = store
            .create_goal_for_surface(input.clone(), context.clone())
            .unwrap();
        store
            .connection()
            .unwrap()
            .execute(
                "UPDATE goal_surface_outbox SET command_digest = ?1
                 WHERE store_commit_id = ?2",
                params![[99_u8; 32].as_slice(), context.store_commit_id],
            )
            .unwrap();
        let command_digest_error = store
            .pending_surface_mutations("corrupted-surface-outbox")
            .unwrap_err();
        assert!(
            command_digest_error
                .to_string()
                .contains("canonical digest")
        );
        store
            .connection()
            .unwrap()
            .execute(
                "UPDATE goal_surface_outbox SET command_digest = ?1
                 WHERE store_commit_id = ?2",
                params![context.command_digest.as_slice(), context.store_commit_id],
            )
            .unwrap();
        let mut corrupted = created.clone();
        let GoalSurfaceRowState::Present(goal) = &mut corrupted.receipt.row_state else {
            panic!("created surface mutation must contain the Goal row");
        };
        goal.objective = "tampered objective".to_string();
        store
            .connection()
            .unwrap()
            .execute(
                "UPDATE goal_surface_outbox SET payload_json = ?1
                 WHERE store_commit_id = ?2",
                params![
                    serde_json::to_string(&corrupted).unwrap(),
                    context.store_commit_id
                ],
            )
            .unwrap();

        let scan_error = store
            .pending_surface_mutations("corrupted-surface-outbox")
            .unwrap_err();
        assert!(scan_error.to_string().contains("canonical digest"));
        let replay_error = store
            .create_goal_for_surface(input, context.clone())
            .unwrap_err();
        assert!(replay_error.to_string().contains("canonical digest"));
        let acknowledge_error = store
            .acknowledge_surface_mutation(
                &context.store_commit_id,
                &created.receipt.receipt_digest,
                owner_epoch,
            )
            .unwrap_err();
        assert!(acknowledge_error.to_string().contains("canonical digest"));
    }

    #[test]
    fn surface_adoption_preserves_a_legacy_goal_and_creates_the_recovery_outbox() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let legacy = store
            .create_goal(CreateGoalInput {
                session_id: "legacy-surface-goal".to_string(),
                objective: "preserve the existing Goal".to_string(),
                token_budget: Some(7_000),
                now: 50,
            })
            .unwrap();
        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let context = surface_context(
            "019f8b4d-7d73-7b52-8f44-2cfeac060102",
            [22; 32],
            owner_epoch,
        );

        let adopted = store
            .adopt_goal_for_surface("legacy-surface-goal", context.clone())
            .unwrap();

        assert!(matches!(adopted.mutation, GoalSurfaceMutation::Created));
        assert_eq!(adopted.receipt.goal_id, legacy.goal_id);
        assert_eq!(adopted.receipt.goal_revision, 1);
        assert_eq!(
            adopted.receipt.objective_revision,
            legacy.objective_revision
        );
        assert_eq!(adopted.receipt.goal_owner_epoch, owner_epoch);
        assert!(matches!(
            adopted.receipt.row_state,
            GoalSurfaceRowState::Present(ref goal)
                if goal.objective == "preserve the existing Goal"
                    && goal.token_budget == Some(7_000)
        ));
        assert_eq!(
            store
                .adopt_goal_for_surface("legacy-surface-goal", context)
                .unwrap(),
            adopted,
            "the adoption commit identity must replay exactly"
        );
        assert_eq!(
            store
                .pending_surface_mutations("legacy-surface-goal")
                .unwrap(),
            vec![adopted]
        );
    }

    #[test]
    fn surface_adoption_closes_an_unowned_quiescent_legacy_run() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let legacy = create_goal(&store, "legacy-quiescent-run");
        let goal_run_id = GoalRunId::new();
        let outer_turn_id = GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: legacy.goal_id.clone(),
                goal_run_id: goal_run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 60,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: legacy.goal_id.clone(),
                goal_run_id: goal_run_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "legacy-provider-turn".to_string(),
                started_at: 61,
            })
            .unwrap();
        store
            .finish_outer_turn(FinishOuterTurnInput {
                goal_id: legacy.goal_id,
                goal_run_id,
                outer_turn_id,
                status: GoalTurnStatus::Success,
                tool_count: 0,
                model_response_count: 1,
                gap_fingerprint: None,
                usage_event: None,
                finished_at: 62,
            })
            .unwrap();
        assert!(
            store
                .get_by_session("legacy-quiescent-run")
                .unwrap()
                .unwrap()
                .current_run
                .is_some()
        );

        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let adopted = store
            .adopt_goal_for_surface(
                "legacy-quiescent-run",
                surface_context(
                    "019f8b4d-7d73-7b52-8f44-2cfeac060103",
                    [23; 32],
                    owner_epoch,
                ),
            )
            .unwrap();

        assert!(matches!(
            adopted.receipt.row_state,
            GoalSurfaceRowState::Present(GoalRecord {
                state: GoalState::Paused {
                    reason: GoalPauseReason::Recovery,
                    ..
                },
                current_run: None,
                ..
            })
        ));
    }

    #[test]
    fn surface_clear_keeps_a_restart_recoverable_tombstone_after_goal_deletion() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals.sqlite3");
        let store = GoalStore::open(&path).unwrap();
        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let created = store
            .create_goal_for_surface(
                CreateGoalInput {
                    session_id: "surface-clear".to_string(),
                    objective: "remove without losing the terminal receipt".to_string(),
                    token_budget: None,
                    now: 100,
                },
                surface_context("019f8b4d-7d73-7b52-8f44-2cfeac060002", [3; 32], owner_epoch),
            )
            .unwrap();
        let removed = store
            .clear_goal_for_surface(
                "surface-clear",
                &created.receipt.goal_id,
                created.receipt.goal_revision,
                surface_context("019f8b4d-7d73-7b52-8f44-2cfeac060003", [4; 32], owner_epoch),
            )
            .unwrap();

        assert!(store.get_by_session("surface-clear").unwrap().is_none());
        assert_eq!(removed.receipt.goal_id, created.receipt.goal_id);
        assert_eq!(removed.receipt.goal_revision, 2);
        assert_eq!(removed.receipt.catalog_revision, 2);
        assert_eq!(
            removed.receipt.goal_owner_epoch, created.receipt.goal_owner_epoch,
            "a restart cannot silently replace the durable Goal owner epoch"
        );
        assert!(matches!(
            removed.mutation,
            GoalSurfaceMutation::Removed {
                previous_revision: 1,
                tombstone_revision: 2,
            }
        ));
        assert!(matches!(
            removed.receipt.row_state,
            GoalSurfaceRowState::Removed
        ));

        drop(store);
        let reopened = GoalStore::open(&path).unwrap();
        assert_eq!(
            reopened.pending_surface_mutations("surface-clear").unwrap(),
            vec![created, removed]
        );
    }

    #[test]
    fn surface_edit_requires_the_exact_fence_and_preserves_the_token_budget() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let owner_epoch = store.claim_surface_owner_epoch().unwrap();
        let created = store
            .create_goal_for_surface(
                CreateGoalInput {
                    session_id: "surface-edit".to_string(),
                    objective: "original objective".to_string(),
                    token_budget: Some(8_000),
                    now: 100,
                },
                surface_context("019f8b4d-7d73-7b52-8f44-2cfeac060004", [5; 32], owner_epoch),
            )
            .unwrap();

        let stale = store
            .edit_goal_for_surface(
                "surface-edit",
                &created.receipt.goal_id,
                created.receipt.goal_revision + 1,
                "must not commit",
                GoalSurfaceTokenBudgetUpdate::Keep,
                101,
                surface_context("019f8b4d-7d73-7b52-8f44-2cfeac060005", [6; 32], owner_epoch),
            )
            .unwrap_err();
        assert!(stale.to_string().contains("stale"));
        assert_eq!(
            store
                .get_by_session("surface-edit")
                .unwrap()
                .unwrap()
                .objective,
            "original objective"
        );

        let edited = store
            .edit_goal_for_surface(
                "surface-edit",
                &created.receipt.goal_id,
                created.receipt.goal_revision,
                "edited objective",
                GoalSurfaceTokenBudgetUpdate::Keep,
                102,
                surface_context("019f8b4d-7d73-7b52-8f44-2cfeac060006", [7; 32], owner_epoch),
            )
            .unwrap();

        assert_eq!(edited.receipt.goal_revision, 2);
        assert_eq!(edited.receipt.objective_revision, 2);
        assert_eq!(
            edited.receipt.catalog_revision,
            created.receipt.catalog_revision
        );
        assert_eq!(
            edited.receipt.goal_owner_epoch,
            created.receipt.goal_owner_epoch
        );
        assert!(matches!(
            edited.mutation,
            GoalSurfaceMutation::Edited {
                previous_revision: 1
            }
        ));
        assert!(matches!(
            edited.receipt.row_state,
            GoalSurfaceRowState::Present(ref goal)
                if goal.objective == "edited objective" && goal.token_budget == Some(8_000)
        ));

        let same_objective = store
            .edit_goal_for_surface(
                "surface-edit",
                &edited.receipt.goal_id,
                edited.receipt.goal_revision,
                "edited objective",
                GoalSurfaceTokenBudgetUpdate::Set(Some(9_000)),
                103,
                surface_context("019f8b4d-7d73-7b52-8f44-2cfeac060007", [8; 32], owner_epoch),
            )
            .unwrap();

        assert_eq!(same_objective.receipt.goal_revision, 3);
        assert_eq!(
            same_objective.receipt.objective_revision, 2,
            "changing only the token budget must not invent an objective revision"
        );
        assert!(matches!(
            same_objective.receipt.row_state,
            GoalSurfaceRowState::Present(ref goal)
                if goal.objective == "edited objective" && goal.token_budget == Some(9_000)
        ));
    }

    #[test]
    fn usage_event_is_idempotent_and_does_not_double_count_cache_tokens() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-usage");
        let event = GoalUsageEvent {
            usage_event_id: "generation-1:model".to_string(),
            goal_id: goal.goal_id,
            source: "model".to_string(),
            usage: GoalUsage {
                charged_input_tokens: 100,
                output_tokens: 20,
                cache_tokens: 80,
                verifier_tokens: 0,
                cost_micros: 12,
                elapsed_seconds: 3,
            },
            created_at: 101,
        };

        let first = store.record_usage_once(event.clone()).unwrap();
        let second = store.record_usage_once(event).unwrap();
        let projected = store.project_thread_goal("session-usage").unwrap().unwrap();

        assert_eq!(first, second);
        assert_eq!(first.charged_tokens(), 120);
        assert_eq!(projected.tokens_used, 120);
        assert_eq!(projected.time_used_seconds, 3);
    }

    #[test]
    fn concurrent_usage_writers_preserve_every_unique_event() {
        let dir = tempdir().unwrap();
        let store = Arc::new(GoalStore::open(dir.path().join("goals.sqlite3")).unwrap());
        let goal = create_goal(&store, "session-concurrent");
        let mut workers = Vec::new();

        for index in 0..8 {
            let store = Arc::clone(&store);
            let goal_id = goal.goal_id.clone();
            workers.push(thread::spawn(move || {
                store
                    .record_usage_once(GoalUsageEvent {
                        usage_event_id: format!("generation-{index}:model"),
                        goal_id,
                        source: "model".to_string(),
                        usage: GoalUsage {
                            charged_input_tokens: 10,
                            output_tokens: 1,
                            ..GoalUsage::default()
                        },
                        created_at: 200 + index,
                    })
                    .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let projection = store
            .project_thread_goal("session-concurrent")
            .unwrap()
            .unwrap();
        assert_eq!(projection.tokens_used, 88);
        assert_eq!(store.usage_event_count(&goal.goal_id).unwrap(), 8);
    }

    #[test]
    fn reopening_recovers_in_flight_run_to_paused_recovery() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals.sqlite3");
        let store = GoalStore::open(&path).unwrap();
        let goal = create_goal(&store, "session-recovery");
        let run_id = GoalRunId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 300,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: orca_core::goal_runtime::GoalOuterTurnId::new(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "turn-provider-1".to_string(),
                started_at: 301,
            })
            .unwrap();
        drop(store);

        let reopened = GoalStore::open(path).unwrap();
        reopened.recover_in_flight_runs().unwrap();
        let recovered = reopened
            .get_by_session("session-recovery")
            .unwrap()
            .unwrap();

        assert!(matches!(
            recovered.state,
            GoalState::Paused {
                reason: GoalPauseReason::Recovery,
                ..
            }
        ));
        assert_eq!(reopened.in_flight_run_count().unwrap(), 0);
        assert!(reopened.transition_count(&goal.goal_id).unwrap() >= 2);
    }

    #[test]
    fn opening_a_reader_does_not_recover_a_live_in_flight_run() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals.sqlite3");
        let owner = GoalStore::open(&path).unwrap();
        let goal = create_goal(&owner, "session-live-owner");
        let run_id = GoalRunId::new();
        owner
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 400,
            })
            .unwrap();
        owner
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id,
                goal_run_id: run_id,
                outer_turn_id: GoalOuterTurnId::new(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "live-provider-turn".to_string(),
                started_at: 401,
            })
            .unwrap();

        let reader = GoalStore::open(&path).unwrap();
        let record = reader
            .get_by_session("session-live-owner")
            .unwrap()
            .unwrap();

        assert_eq!(record.state, GoalState::Active);
        assert!(record.current_run.as_ref().is_some_and(|run| run.in_flight));
        assert_eq!(reader.in_flight_run_count().unwrap(), 1);
    }

    #[test]
    fn detached_controls_cannot_mutate_an_in_flight_goal() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-detached-control");
        let run_id = GoalRunId::new();
        let outer_turn_id = GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 500,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id,
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "detached-control-turn".to_string(),
                started_at: 501,
            })
            .unwrap();

        assert!(matches!(
            store.edit_goal(
                "session-detached-control",
                "unsafe detached edit",
                None,
                502,
            ),
            Err(GoalStoreError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            store.resume_into(
                "session-detached-control",
                "session-detached-resume",
                503,
            ),
            Err(GoalStoreError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            store.clear_goal("session-detached-control"),
            Err(GoalStoreError::Invalid(message)) if message.contains("in flight")
        ));
        assert_eq!(
            store.outer_turn_status(&outer_turn_id).unwrap().as_deref(),
            Some("in_flight")
        );
        assert_eq!(store.in_flight_run_count().unwrap(), 1);
        assert_eq!(
            store
                .get_by_session("session-detached-control")
                .unwrap()
                .unwrap()
                .objective,
            "ship runtime-owned goals"
        );
    }

    #[test]
    fn legacy_json_migrates_once_and_is_backed_up_after_commit() {
        let dir = tempdir().unwrap();
        let legacy_path = dir.path().join("goals_1.json");
        let db_path = dir.path().join("goals.sqlite3");
        let legacy_goal = ThreadGoal {
            session_id: "legacy-session".to_string(),
            objective: "preserve legacy goal".to_string(),
            status: ThreadGoalStatus::Stalled,
            token_budget: Some(50_000),
            tokens_used: 123,
            time_used_seconds: 45,
            created_at: 10,
            updated_at: 20,
        };
        let mut goals = BTreeMap::new();
        goals.insert(legacy_goal.session_id.clone(), legacy_goal);
        fs::write(
            &legacy_path,
            serde_json::to_vec_pretty(&serde_json::json!({"goals": goals})).unwrap(),
        )
        .unwrap();

        let store = GoalStore::open_with_legacy(&db_path, &legacy_path).unwrap();
        let migrated = store.get_by_session("legacy-session").unwrap().unwrap();

        assert!(matches!(
            migrated.state,
            GoalState::Paused {
                reason: GoalPauseReason::NoProgress,
                ..
            }
        ));
        assert_eq!(migrated.usage.charged_tokens(), 123);
        assert!(!legacy_path.exists());
        let backups = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("goals_1.migrated")
            })
            .count();
        assert_eq!(backups, 1);

        drop(store);
        let reopened = GoalStore::open_with_legacy(db_path, legacy_path).unwrap();
        assert_eq!(reopened.goal_count().unwrap(), 1);
    }

    #[test]
    fn malformed_legacy_json_is_preserved_and_migration_fails_closed() {
        let dir = tempdir().unwrap();
        let legacy_path = dir.path().join("goals_1.json");
        let db_path = dir.path().join("goals.sqlite3");
        fs::write(&legacy_path, "{not valid JSON").unwrap();

        let error = GoalStore::open_with_legacy(db_path, &legacy_path).unwrap_err();

        assert!(error.to_string().contains("legacy goal migration"));
        assert_eq!(fs::read_to_string(legacy_path).unwrap(), "{not valid JSON");
    }

    #[test]
    fn intent_record_is_idempotent_and_preserves_typed_ack() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-intent");
        let run_id = GoalRunId::new();
        let outer_turn_id = orca_core::goal_runtime::GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 400,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "turn-provider-intent".to_string(),
                started_at: 401,
            })
            .unwrap();
        let intent_id = IntentId::new();
        let intent = GoalUpdateIntent {
            intent_id: intent_id.clone(),
            requested_state: GoalRequestedState::Complete,
            reason: "verified".to_string(),
            evidence: vec![EvidenceItem::observation("focused tests passed")],
            blocker: None,
        };
        let ack = GoalUpdateAck::DeferredToTurnEnd {
            intent_id,
            pending_depth: 1,
        };
        let record = GoalIntentRecord {
            outer_turn_id,
            intent,
            ack: ack.clone(),
            created_at: 402,
        };

        assert_eq!(store.record_intent(record.clone()).unwrap(), ack);
        assert_eq!(store.record_intent(record).unwrap(), ack);
        assert_eq!(store.intent_count().unwrap(), 1);
    }

    #[test]
    fn finishing_outer_turn_commits_usage_and_releases_in_flight_run() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-finish");
        let run_id = GoalRunId::new();
        let outer_turn_id = orca_core::goal_runtime::GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 500,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "turn-provider-finish".to_string(),
                started_at: 501,
            })
            .unwrap();

        let outcome = store
            .finish_outer_turn(FinishOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id.clone(),
                status: GoalTurnStatus::Success,
                tool_count: 4,
                model_response_count: 3,
                gap_fingerprint: Some("roadmap:next-slice".to_string()),
                usage_event: Some(GoalUsageEvent {
                    usage_event_id: "generation-finish:model".to_string(),
                    goal_id: goal.goal_id.clone(),
                    source: "model".to_string(),
                    usage: GoalUsage {
                        charged_input_tokens: 25,
                        output_tokens: 5,
                        elapsed_seconds: 2,
                        ..GoalUsage::default()
                    },
                    created_at: 502,
                }),
                finished_at: 503,
            })
            .unwrap();

        assert!(!outcome.already_finished);
        assert_eq!(outcome.usage.charged_tokens(), 30);
        assert_eq!(store.in_flight_run_count().unwrap(), 0);
        assert_eq!(
            store.outer_turn_status(&outer_turn_id).unwrap().as_deref(),
            Some("success")
        );
    }

    #[test]
    fn recent_gap_history_preserves_progress_barriers() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-gap-barrier");
        let run_id = GoalRunId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 700,
            })
            .unwrap();

        for (index, fingerprint) in [
            Some("gap:alpha".to_string()),
            None,
            Some("gap:alpha".to_string()),
        ]
        .into_iter()
        .enumerate()
        {
            let outer_turn_id = GoalOuterTurnId::new();
            let at = 701 + i64::try_from(index).unwrap() * 2;
            store
                .begin_outer_turn(BeginOuterTurnInput {
                    goal_id: goal.goal_id.clone(),
                    goal_run_id: run_id.clone(),
                    outer_turn_id: outer_turn_id.clone(),
                    origin: GoalTurnOrigin::Continuation,
                    provider_turn_id: format!("provider-gap-{index}"),
                    started_at: at,
                })
                .unwrap();
            store
                .finish_outer_turn(FinishOuterTurnInput {
                    goal_id: goal.goal_id.clone(),
                    goal_run_id: run_id.clone(),
                    outer_turn_id,
                    status: GoalTurnStatus::Success,
                    tool_count: 1,
                    model_response_count: 1,
                    gap_fingerprint: fingerprint,
                    usage_event: None,
                    finished_at: at + 1,
                })
                .unwrap();
        }

        assert_eq!(
            store.recent_gap_fingerprints(&goal.goal_id, 3).unwrap(),
            vec![
                Some("gap:alpha".to_string()),
                None,
                Some("gap:alpha".to_string()),
            ]
        );
    }

    #[test]
    fn verifier_usage_is_charged_once_to_goal_and_outer_turn() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-verifier-usage");
        let run_id = GoalRunId::new();
        let outer_turn_id = GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 510,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "turn-provider-verifier".to_string(),
                started_at: 511,
            })
            .unwrap();
        store
            .finish_outer_turn(FinishOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id.clone(),
                status: GoalTurnStatus::Success,
                tool_count: 1,
                model_response_count: 1,
                gap_fingerprint: None,
                usage_event: None,
                finished_at: 512,
            })
            .unwrap();
        let event = GoalUsageEvent {
            usage_event_id: format!("verifier:{outer_turn_id}:1"),
            goal_id: goal.goal_id.clone(),
            source: "goal_verifier".to_string(),
            usage: GoalUsage {
                verifier_tokens: 17,
                cost_micros: 4,
                elapsed_seconds: 1,
                ..GoalUsage::default()
            },
            created_at: 513,
        };

        let first = store
            .record_verifier_usage_once(&outer_turn_id, event.clone())
            .unwrap();
        let second = store
            .record_verifier_usage_once(&outer_turn_id, event)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(first.verifier_tokens, 17);
        assert_eq!(store.usage_event_count(&goal.goal_id).unwrap(), 1);
        assert_eq!(
            store.outer_turn_verifier_tokens(&outer_turn_id).unwrap(),
            Some(17)
        );
        assert_eq!(
            store.audit_snapshot(&goal.goal_id).unwrap(),
            GoalAuditSnapshot {
                outer_turns: 1,
                intents: 0,
                usage_events: 1,
                verifier_tokens: 17,
                transitions: 1,
                in_flight_runs: 0,
            }
        );
    }

    #[test]
    fn finishing_outer_turn_at_budget_boundary_pauses_continuation() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = store
            .create_goal(CreateGoalInput {
                session_id: "session-budget-boundary".to_string(),
                objective: "stop exactly at the budget".to_string(),
                token_budget: Some(100),
                now: 600,
            })
            .unwrap();
        let run_id = GoalRunId::new();
        let outer_turn_id = orca_core::goal_runtime::GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 601,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "provider-budget-boundary".to_string(),
                started_at: 602,
            })
            .unwrap();

        store
            .finish_outer_turn(FinishOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id,
                status: GoalTurnStatus::Success,
                tool_count: 1,
                model_response_count: 1,
                gap_fingerprint: None,
                usage_event: Some(GoalUsageEvent {
                    usage_event_id: "budget-boundary:model".to_string(),
                    goal_id: goal.goal_id.clone(),
                    source: "model".to_string(),
                    usage: GoalUsage {
                        charged_input_tokens: 70,
                        output_tokens: 30,
                        ..GoalUsage::default()
                    },
                    created_at: 603,
                }),
                finished_at: 603,
            })
            .unwrap();

        let record = store
            .get_by_session("session-budget-boundary")
            .unwrap()
            .unwrap();
        assert_eq!(record.state, GoalState::BudgetLimited);
        assert_eq!(record.usage.charged_tokens(), 100);
        assert_eq!(store.in_flight_run_count().unwrap(), 0);
    }

    #[test]
    fn failed_state_transition_rolls_back_without_extra_history() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-rollback");
        let before = store.transition_count(&goal.goal_id).unwrap();

        let error = store
            .transition_state(
                &GoalId::new(),
                GoalState::Paused {
                    reason: GoalPauseReason::User,
                    message: "pause".to_string(),
                },
                "user_paused",
                None,
                600,
            )
            .unwrap_err();

        assert!(error.to_string().contains("goal"));
        assert_eq!(store.transition_count(&goal.goal_id).unwrap(), before);
    }
}
