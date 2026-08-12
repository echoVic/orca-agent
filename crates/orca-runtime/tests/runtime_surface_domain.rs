use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::CancelToken;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_runtime::goal_store::{CreateGoalInput, GoalStore};
use orca_runtime::runtime_host::{
    GenerationContext, HostedTurnRequest, RuntimeHost, ThreadOperationExecutor,
    ThreadOperationOutcome,
};
use orca_runtime::surface::{
    AttachResult, DisplayText, ExpectedGoal, FreshAttachRequest, GoalMutationAction, GoalRunInput,
    MutationReply, NonEmptyText, NonEmptyVec, SurfaceAttachmentRole, SurfaceCapability,
    SurfaceInputRequest, SurfaceInputRequestBlock, SurfaceInteractionKind, SurfaceOperationId,
    SurfaceRequestId,
};
use orca_runtime::thread::RuntimeThread;
use tempfile::tempdir;

static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct GoalExecutionGate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
    calls: AtomicUsize,
}

#[derive(Default)]
struct GoalCancellationGate {
    entered: Mutex<bool>,
    changed: Condvar,
}

impl GoalCancellationGate {
    fn wait_until_entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut entered = self.entered.lock().unwrap();
        while !*entered {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "Goal executor was not admitted");
            let (next, timeout) = self.changed.wait_timeout(entered, remaining).unwrap();
            entered = next;
            assert!(!timeout.timed_out(), "Goal executor was not admitted");
        }
    }
}

impl ThreadOperationExecutor for GoalCancellationGate {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        _request: &HostedTurnRequest,
        _generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        {
            let mut entered = self.entered.lock().unwrap();
            *entered = true;
            self.changed.notify_all();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cancel.is_cancelled() {
            assert!(
                Instant::now() < deadline,
                "Goal operation was not cancelled by typed pause"
            );
            std::thread::yield_now();
        }
        thread.lifecycle_mut().finish_task(RunStatus::Cancelled);
        Ok(RunStatus::Cancelled.into())
    }
}

impl GoalExecutionGate {
    fn wait_until_entered(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = self.state.lock().unwrap();
        while !state.0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "Goal executor was not admitted");
            let (next, timeout) = self.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            assert!(!timeout.timed_out(), "Goal executor was not admitted");
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.1 = true;
        self.changed.notify_all();
    }
}

impl ThreadOperationExecutor for GoalExecutionGate {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        _request: &HostedTurnRequest,
        _generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn io::Write + Send),
        _cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call >= 3 {
            thread.lifecycle_mut().finish_task(RunStatus::Failed);
            return Ok(RunStatus::Failed.into());
        }
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
        thread.lifecycle_mut().finish_task(RunStatus::Success);
        Ok(RunStatus::Success.into())
    }
}

#[test]
fn goal_set_and_run_commits_the_goal_store_binding_and_requested_operation_atomically() {
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let previous = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };

    let executor = Arc::new(GoalExecutionGate::default());
    let host = RuntimeHost::start_with_executor(executor.clone()).unwrap();
    let thread = host
        .surface_handle()
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "Goal set and run",
        )
        .unwrap();
    let session_id = thread.thread_id().to_string();
    let attachment = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::ManageGoal,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("fresh TUI attachment failed"),
    };

    let output = match attachment
        .client
        .goal_mutation(
            SurfaceRequestId::new(),
            GoalMutationAction::SetAndRun {
                expected_goal: ExpectedGoal::None,
                objective: NonEmptyText::try_new("ship one typed Goal operation").unwrap(),
                token_budget: Some(20_000),
                input: GoalRunInput::Supplied {
                    request: SurfaceInputRequest {
                        blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                            text: DisplayText::new("ship one typed Goal operation"),
                        }])
                        .unwrap(),
                    },
                },
            },
        )
        .expect("Goal mutation command")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("Goal set-and-run must commit"),
    };

    let operation_id = output
        .operation_id
        .clone()
        .expect("set-and-run reserves one operation");
    executor.wait_until_entered();
    let goal = output.goal.as_ref().expect("set-and-run returns the Goal");
    assert_eq!(
        goal.current_run
            .as_ref()
            .expect("Goal has a preparing run")
            .operation_id,
        operation_id
    );
    let snapshot = attach_snapshot(&thread.surface());
    assert_eq!(snapshot.goal.as_ref(), output.goal.as_ref());
    assert!(
        snapshot
            .foreground_operation
            .as_ref()
            .is_some_and(|operation| {
                operation.operation_id == operation_id
                    && matches!(
                        operation.intent.kind,
                        orca_runtime::surface::OperationKind::GoalRun { .. }
                    )
                    && operation
                        .generations
                        .first()
                        .is_some_and(|generation| generation.goal_identity.is_some())
            }),
        "set-and-run must internally admit the same Goal operation with its Goal identity"
    );
    let goal_store = GoalStore::load_default().unwrap();
    let stored_operation_id: String = rusqlite::Connection::open(goal_store.path())
        .unwrap()
        .query_row(
            "SELECT surface_operation_id
             FROM goal_runs
             WHERE goal_id = ?1 AND finished_at IS NULL",
            [output.goal_receipt.goal_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_operation_id,
        surface_operation_id_text(&operation_id)
    );
    assert!(
        GoalStore::load_default()
            .unwrap()
            .pending_surface_mutations(thread.thread_id())
            .unwrap()
            .is_empty(),
        "the outbox is acknowledged only after the atomic surface batch commits"
    );

    executor.release();
    let terminal = attachment
        .client
        .wait_operation_terminal(SurfaceRequestId::new(), operation_id.clone())
        .expect("Goal operation terminal waiter");
    assert!(
        matches!(
            terminal,
            orca_runtime::surface::WaitOperationTerminalResult::Terminal { .. }
        ),
        "the Goal operation must terminalize through the typed surface"
    );
    assert_eq!(
        executor.calls.load(Ordering::SeqCst),
        3,
        "three identical model-fixable gaps must pause instead of admitting another successor"
    );
    let settled = attach_snapshot(&thread.surface());
    let settled_goal = settled.goal.as_ref().expect("settled Goal");
    assert!(
        settled_goal.current_run.is_none(),
        "natural operation terminalization must close the Goal run without restart"
    );
    assert!(matches!(
        settled_goal.state,
        orca_runtime::surface::SurfaceGoalState::Paused {
            reason: orca_runtime::surface::SurfaceGoalPauseReason::NoProgress,
            ..
        }
    ));
    let set_again_digest = GoalStore::load_default()
        .unwrap()
        .current_surface_receipt_digest(thread.thread_id())
        .unwrap()
        .expect("settled Goal receipt digest");
    assert_eq!(
        settled_goal.receipt_digest.as_bytes(),
        &set_again_digest,
        "the surface projection and durable Goal fence must expose the same receipt"
    );
    let set_again = match attachment
        .client
        .goal_mutation(
            SurfaceRequestId::new(),
            GoalMutationAction::SetAndRun {
                expected_goal: ExpectedGoal::Exact(orca_runtime::surface::SurfaceGoalFence {
                    goal_id: settled_goal.goal_id.clone(),
                    goal_revision: settled_goal.goal_revision,
                    goal_owner_epoch: settled_goal.goal_owner_epoch,
                }),
                objective: NonEmptyText::try_new("ship the replacement typed Goal").unwrap(),
                token_budget: Some(30_000),
                input: GoalRunInput::DerivedFromGoal {
                    goal_id: settled_goal.goal_id.clone(),
                    objective_revision: settled_goal.objective_revision,
                    goal_receipt_digest: orca_runtime::surface::Sha256Digest::new(set_again_digest),
                },
            },
        )
        .expect("typed Goal replacement")
    {
        MutationReply::Committed {
            mutation, value, ..
        } => {
            assert_eq!(
                mutation.acknowledgements.as_slice().len(),
                3,
                "edit, run start, and operation request commit in one batch"
            );
            value
        }
        _ => panic!("Goal replacement set-and-run must commit"),
    };
    let set_again_operation_id = set_again
        .operation_id
        .clone()
        .expect("replacement Goal reserves an operation");
    assert_eq!(
        set_again
            .goal
            .as_ref()
            .expect("replacement Goal remains present")
            .objective
            .as_str(),
        "ship the replacement typed Goal"
    );
    assert!(matches!(
        attachment
            .client
            .wait_operation_terminal(SurfaceRequestId::new(), set_again_operation_id.clone())
            .expect("replacement Goal terminal waiter"),
        orca_runtime::surface::WaitOperationTerminalResult::Terminal { .. }
    ));
    let set_again_settled = attach_snapshot(&thread.surface());
    let set_again_goal = set_again_settled
        .goal
        .as_ref()
        .expect("replacement Goal settled");
    let resume_digest = GoalStore::load_default()
        .unwrap()
        .current_surface_receipt_digest(thread.thread_id())
        .unwrap()
        .expect("settled Goal receipt digest");
    let resumed_run = match attachment
        .client
        .goal_mutation(
            SurfaceRequestId::new(),
            GoalMutationAction::ResumeAndRun {
                fence: orca_runtime::surface::SurfaceGoalFence {
                    goal_id: set_again_goal.goal_id.clone(),
                    goal_revision: set_again_goal.goal_revision,
                    goal_owner_epoch: set_again_goal.goal_owner_epoch,
                },
                input: GoalRunInput::DerivedFromGoal {
                    goal_id: set_again_goal.goal_id.clone(),
                    objective_revision: set_again_goal.objective_revision,
                    goal_receipt_digest: orca_runtime::surface::Sha256Digest::new(resume_digest),
                },
            },
        )
        .expect("typed Goal resume")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("Goal resume must commit"),
    };
    let resumed_operation_id = resumed_run
        .operation_id
        .clone()
        .expect("Goal resume creates an operation");
    assert_ne!(resumed_operation_id, operation_id);
    let resumed_terminal = attachment
        .client
        .wait_operation_terminal(SurfaceRequestId::new(), resumed_operation_id.clone())
        .expect("resumed Goal terminal waiter");
    assert!(matches!(
        resumed_terminal,
        orca_runtime::surface::WaitOperationTerminalResult::Terminal { .. }
    ));
    let resumed_settled = attach_snapshot(&thread.surface());
    assert!(
        resumed_settled
            .goal
            .as_ref()
            .is_some_and(|goal| goal.current_run.is_none())
    );
    host.shutdown().unwrap();

    let host = RuntimeHost::start().unwrap();
    let resumed = host
        .surface_handle()
        .start_thread(
            test_config(
                cwd.path().to_path_buf(),
                HistoryMode::Resume(session_id.clone()),
            ),
            "recovered Goal set and run",
        )
        .unwrap();
    let recovered = attach_snapshot(&resumed.surface());
    let recovered_goal = recovered.goal.as_ref().expect("recovered Goal");
    assert!(
        recovered_goal.current_run.is_none(),
        "restart must close the preparing Goal run after its operation is terminalized"
    );
    assert!(matches!(
        recovered_goal.state,
        orca_runtime::surface::SurfaceGoalState::Paused {
            reason: orca_runtime::surface::SurfaceGoalPauseReason::NoProgress,
            ..
        }
    ));
    assert!(
        recovered
            .operation_history
            .iter()
            .any(|operation| operation.operation_id == operation_id && operation.terminal.is_some()),
        "the matching requested operation must be terminal after restart"
    );
    assert!(
        recovered.operation_history.iter().any(|operation| {
            operation.operation_id == set_again_operation_id && operation.terminal.is_some()
        }),
        "the replacement Goal operation must survive restart as terminal"
    );
    assert!(
        recovered.operation_history.iter().any(|operation| {
            operation.operation_id == resumed_operation_id && operation.terminal.is_some()
        }),
        "the resumed Goal operation must also survive restart as terminal"
    );
    assert!(
        GoalStore::load_default()
            .unwrap()
            .pending_surface_mutations(&session_id)
            .unwrap()
            .is_empty(),
        "Goal recovery is acknowledged only after its typed patch commits"
    );
    let resumed_attachment = match resumed.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::ManageGoal,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("resumed TUI attachment failed"),
    };
    let edited = match resumed_attachment
        .client
        .goal_mutation(
            SurfaceRequestId::new(),
            GoalMutationAction::Edit {
                fence: orca_runtime::surface::SurfaceGoalFence {
                    goal_id: recovered_goal.goal_id.clone(),
                    goal_revision: recovered_goal.goal_revision,
                    goal_owner_epoch: recovered_goal.goal_owner_epoch,
                },
                objective: NonEmptyText::try_new("ship the edited typed Goal").unwrap(),
                token_budget: orca_runtime::surface::GoalTokenBudgetUpdate::Keep,
            },
        )
        .expect("typed Goal edit")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("Goal edit must commit"),
    };
    let edited_goal = edited.goal.as_ref().expect("edited Goal remains present");
    assert_eq!(edited_goal.objective.as_str(), "ship the edited typed Goal");
    let cleared = resumed_attachment
        .client
        .goal_mutation(
            SurfaceRequestId::new(),
            GoalMutationAction::Clear {
                fence: orca_runtime::surface::SurfaceGoalFence {
                    goal_id: edited_goal.goal_id.clone(),
                    goal_revision: edited_goal.goal_revision,
                    goal_owner_epoch: edited_goal.goal_owner_epoch,
                },
            },
        )
        .expect("typed Goal clear");
    assert!(matches!(
        cleared,
        MutationReply::Committed {
            value: orca_runtime::surface::GoalMutationOutput { goal: None, .. },
            ..
        }
    ));
    host.shutdown().unwrap();

    match previous {
        Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn goal_pause_commits_goal_state_and_operation_cancellation_before_terminal_wake() {
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let previous = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };

    let executor = Arc::new(GoalCancellationGate::default());
    let host = RuntimeHost::start_with_executor(executor.clone()).unwrap();
    let thread = host
        .surface_handle()
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "Goal pause",
        )
        .unwrap();
    let session_id = thread.thread_id().to_string();
    let attachment = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
            SurfaceCapability::ManageGoal,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("fresh TUI attachment failed"),
    };
    let started = match attachment
        .client
        .goal_mutation(
            SurfaceRequestId::new(),
            GoalMutationAction::SetAndRun {
                expected_goal: ExpectedGoal::None,
                objective: NonEmptyText::try_new("pause this typed Goal").unwrap(),
                token_budget: None,
                input: GoalRunInput::Supplied {
                    request: SurfaceInputRequest {
                        blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                            text: DisplayText::new("pause this typed Goal"),
                        }])
                        .unwrap(),
                    },
                },
            },
        )
        .expect("Goal mutation command")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("Goal set-and-run must commit"),
    };
    let operation_id = started.operation_id.expect("Goal operation");
    executor.wait_until_entered();
    let live_goal = attach_snapshot(&thread.surface())
        .goal
        .clone()
        .expect("live Goal");
    let paused = match attachment
        .client
        .pause_goal_operation(
            SurfaceRequestId::new(),
            orca_runtime::surface::SurfaceGoalFence {
                goal_id: live_goal.goal_id.clone(),
                goal_revision: live_goal.goal_revision,
                goal_owner_epoch: live_goal.goal_owner_epoch,
            },
        )
        .expect("typed Goal pause")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("Goal pause must commit"),
    };
    assert!(matches!(
        paused.goal.state,
        orca_runtime::surface::SurfaceGoalState::Paused {
            reason: orca_runtime::surface::SurfaceGoalPauseReason::User,
            ..
        }
    ));
    assert!(matches!(
        paused.operation,
        orca_runtime::surface::PauseGoalOperationOutput::Cancelling {
            operation_id: ref cancelling,
            ..
        } if cancelling == &operation_id
    ));
    assert_eq!(
        attach_snapshot(&thread.surface()).goal,
        Some(paused.goal.clone()),
        "the paused Goal must be visible before the command reply"
    );

    let terminal = attachment
        .client
        .wait_operation_terminal(SurfaceRequestId::new(), operation_id.clone())
        .expect("Goal pause terminal waiter");
    let orca_runtime::surface::WaitOperationTerminalResult::Terminal { value } = terminal else {
        panic!("Goal pause must reach a durable terminal");
    };
    assert!(matches!(
        value.terminal,
        orca_runtime::surface::OperationTerminal::Cancelled {
            reason: orca_runtime::surface::CancelReason::GoalPause,
        }
    ));
    let settled = attach_snapshot(&thread.surface());
    assert!(settled.goal.as_ref().is_some_and(|goal| {
        goal.current_run.is_none()
            && matches!(
                goal.state,
                orca_runtime::surface::SurfaceGoalState::Paused {
                    reason: orca_runtime::surface::SurfaceGoalPauseReason::User,
                    ..
                }
            )
    }));
    assert!(
        GoalStore::load_default()
            .unwrap()
            .pending_surface_mutations(&session_id)
            .unwrap()
            .is_empty()
    );
    host.shutdown().unwrap();

    let host = RuntimeHost::start().unwrap();
    let resumed = host
        .surface_handle()
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Resume(session_id)),
            "recovered Goal pause",
        )
        .unwrap();
    let recovered = attach_snapshot(&resumed.surface());
    assert!(recovered.goal.as_ref().is_some_and(|goal| {
        goal.current_run.is_none()
            && matches!(
                goal.state,
                orca_runtime::surface::SurfaceGoalState::Paused {
                    reason: orca_runtime::surface::SurfaceGoalPauseReason::User,
                    ..
                }
            )
    }));
    assert!(recovered.operation_history.iter().any(|operation| {
        operation.operation_id == operation_id
            && matches!(
                operation
                    .terminal
                    .as_ref()
                    .map(|terminal| &terminal.terminal),
                Some(orca_runtime::surface::OperationTerminal::Cancelled {
                    reason: orca_runtime::surface::CancelReason::GoalPause,
                })
            )
    }));
    host.shutdown().unwrap();

    match previous {
        Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn goal_create_is_durable_before_the_facade_replies_and_restores_into_snapshot() {
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().unwrap();
    let home_path = home.path().to_path_buf();
    let cwd = tempdir().unwrap();
    let cwd_path = cwd.path().to_path_buf();
    let previous = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", &home_path) };

    let host = RuntimeHost::start().unwrap();
    let thread = host
        .surface_handle()
        .start_thread(test_config(cwd_path.clone(), HistoryMode::Record), "goal")
        .unwrap();
    let session_id = thread.thread_id().to_string();
    let projected = thread
        .set_goal(
            &session_id,
            "deliver the runtime-owned Goal loop".to_string(),
            100,
        )
        .unwrap();
    assert_eq!(projected.objective, "deliver the runtime-owned Goal loop");

    let first = attach_snapshot(&thread.surface());
    let goal = first.goal.as_ref().expect("committed Goal snapshot");
    assert_eq!(
        goal.objective.as_str(),
        "deliver the runtime-owned Goal loop"
    );
    assert_eq!(goal.goal_revision.get(), 1);

    let edited = thread
        .edit_goal(
            &session_id,
            "deliver and recover the runtime-owned Goal loop".to_string(),
            101,
        )
        .unwrap()
        .expect("existing Goal is edited");
    assert_eq!(
        edited.objective,
        "deliver and recover the runtime-owned Goal loop"
    );
    let edited_snapshot = attach_snapshot(&thread.surface());
    let goal = edited_snapshot.goal.as_ref().expect("edited Goal snapshot");
    assert_eq!(
        goal.objective.as_str(),
        "deliver and recover the runtime-owned Goal loop"
    );
    assert_eq!(goal.goal_revision.get(), 2);
    assert_eq!(goal.objective_revision.get(), 2);
    host.shutdown().unwrap();

    let host = RuntimeHost::start().unwrap();
    let resumed = host
        .surface_handle()
        .start_thread(
            test_config(cwd_path.clone(), HistoryMode::Resume(session_id.clone())),
            "resumed goal",
        )
        .unwrap();
    let restored = attach_snapshot(&resumed.surface());
    let goal = restored.goal.as_ref().expect("restored Goal snapshot");
    assert_eq!(
        goal.objective.as_str(),
        "deliver and recover the runtime-owned Goal loop"
    );
    assert_eq!(goal.goal_revision.get(), 2);

    resumed.clear_goal(&session_id).unwrap();
    assert!(
        attach_snapshot(&resumed.surface()).goal.is_none(),
        "clear must commit the typed Goal tombstone before replying"
    );
    host.shutdown().unwrap();

    let host = RuntimeHost::start().unwrap();
    let resumed = host
        .surface_handle()
        .start_thread(
            test_config(cwd_path, HistoryMode::Resume(session_id)),
            "resumed cleared goal",
        )
        .unwrap();
    assert!(
        attach_snapshot(&resumed.surface()).goal.is_none(),
        "the typed Goal tombstone must survive restart"
    );
    host.shutdown().unwrap();

    match previous {
        Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn quiescent_goal_pause_commits_without_fabricating_an_operation_and_survives_restart() {
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let previous = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };

    let host = RuntimeHost::start().unwrap();
    let thread = host
        .surface_handle()
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "quiescent Goal pause",
        )
        .unwrap();
    let session_id = thread.thread_id().to_string();
    thread
        .set_goal(
            &session_id,
            "pause without an admitted operation".to_string(),
            200,
        )
        .unwrap();
    let attachment = match thread.surface().attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::ManageGoal,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("fresh TUI attachment failed"),
    };
    let goal = attachment
        .baseline
        .snapshot
        .goal
        .as_ref()
        .expect("quiescent Goal");
    let paused = match attachment
        .client
        .pause_goal_operation(
            SurfaceRequestId::new(),
            orca_runtime::surface::SurfaceGoalFence {
                goal_id: goal.goal_id.clone(),
                goal_revision: goal.goal_revision,
                goal_owner_epoch: goal.goal_owner_epoch,
            },
        )
        .expect("quiescent typed Goal pause")
    {
        MutationReply::Committed { value, .. } => value,
        _ => panic!("quiescent Goal pause must commit"),
    };
    assert!(matches!(
        paused.goal.state,
        orca_runtime::surface::SurfaceGoalState::Paused {
            reason: orca_runtime::surface::SurfaceGoalPauseReason::User,
            ..
        }
    ));
    assert!(matches!(
        paused.operation,
        orca_runtime::surface::PauseGoalOperationOutput::None
    ));
    let snapshot = attach_snapshot(&thread.surface());
    assert!(snapshot.foreground_operation.is_none());
    assert!(snapshot.queued_operations.is_empty());
    assert!(snapshot.operation_history.is_empty());
    host.shutdown().unwrap();

    let host = RuntimeHost::start().unwrap();
    let resumed = host
        .surface_handle()
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Resume(session_id)),
            "recovered quiescent Goal pause",
        )
        .unwrap();
    let recovered = attach_snapshot(&resumed.surface());
    assert!(recovered.goal.as_ref().is_some_and(|goal| {
        goal.current_run.is_none()
            && matches!(
                goal.state,
                orca_runtime::surface::SurfaceGoalState::Paused {
                    reason: orca_runtime::surface::SurfaceGoalPauseReason::User,
                    ..
                }
            )
    }));
    assert!(recovered.foreground_operation.is_none());
    assert!(recovered.queued_operations.is_empty());
    assert!(recovered.operation_history.is_empty());
    host.shutdown().unwrap();

    match previous {
        Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn restart_only_acknowledges_a_goal_receipt_already_present_in_the_surface_ledger() {
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let previous = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };

    let host = RuntimeHost::start().unwrap();
    let thread = host
        .surface_handle()
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "goal ack recovery",
        )
        .unwrap();
    let session_id = thread.thread_id().to_string();
    thread
        .set_goal(
            &session_id,
            "retain the exact Goal receipt".to_string(),
            300,
        )
        .unwrap();
    host.shutdown().unwrap();

    let store = GoalStore::load_default().unwrap();
    let connection = rusqlite::Connection::open(store.path()).unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE goal_surface_outbox SET acknowledged = 0 WHERE session_id = ?1",
                [&session_id],
            )
            .unwrap(),
        1
    );
    drop(connection);
    assert_eq!(
        store.pending_surface_mutations(&session_id).unwrap().len(),
        1
    );
    drop(store);

    let host = RuntimeHost::start().unwrap();
    let resumed = host
        .surface_handle()
        .start_thread(
            test_config(
                cwd.path().to_path_buf(),
                HistoryMode::Resume(session_id.clone()),
            ),
            "recovered Goal acknowledgement",
        )
        .unwrap();
    let snapshot = attach_snapshot(&resumed.surface());
    let goal = snapshot.goal.as_ref().expect("existing Goal receipt");
    assert_eq!(goal.goal_revision.get(), 1);
    assert_eq!(goal.objective.as_str(), "retain the exact Goal receipt");
    host.shutdown().unwrap();

    assert!(
        GoalStore::load_default()
            .unwrap()
            .pending_surface_mutations(&session_id)
            .unwrap()
            .is_empty(),
        "startup must settle the already-projected exact receipt without a duplicate Goal patch"
    );

    match previous {
        Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

#[test]
fn restart_adopts_a_pre_surface_goal_without_rewriting_its_objective() {
    let _lock = ORCA_HOME_TEST_LOCK.lock().unwrap();
    let home = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let previous = std::env::var_os("ORCA_HOME");
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };

    let host = RuntimeHost::start().unwrap();
    let thread = host
        .surface_handle()
        .start_thread(
            test_config(cwd.path().to_path_buf(), HistoryMode::Record),
            "legacy Goal",
        )
        .unwrap();
    let session_id = thread.thread_id().to_string();
    host.shutdown().unwrap();

    GoalStore::load_default()
        .unwrap()
        .create_goal(CreateGoalInput {
            session_id: session_id.clone(),
            objective: "preserve the pre-surface Goal".to_string(),
            token_budget: Some(6_000),
            now: 400,
        })
        .unwrap();

    let host = RuntimeHost::start().unwrap();
    let resumed = host
        .surface_handle()
        .start_thread(
            test_config(
                cwd.path().to_path_buf(),
                HistoryMode::Resume(session_id.clone()),
            ),
            "adopted legacy Goal",
        )
        .unwrap();
    let snapshot = attach_snapshot(&resumed.surface());
    let goal = snapshot.goal.as_ref().expect("adopted Goal snapshot");
    assert_eq!(goal.objective.as_str(), "preserve the pre-surface Goal");
    assert_eq!(goal.token_budget, Some(6_000));
    assert_eq!(goal.goal_revision.get(), 1);
    resumed
        .edit_goal(
            &session_id,
            "continue after typed adoption".to_string(),
            401,
        )
        .unwrap()
        .expect("adopted Goal remains editable");
    assert_eq!(
        attach_snapshot(&resumed.surface())
            .goal
            .as_ref()
            .expect("edited adopted Goal")
            .goal_revision
            .get(),
        2
    );
    host.shutdown().unwrap();

    match previous {
        Some(previous) => unsafe { std::env::set_var("ORCA_HOME", previous) },
        None => unsafe { std::env::remove_var("ORCA_HOME") },
    }
}

fn attach_snapshot(
    surface: &orca_runtime::surface::RuntimeSurfaceHandle,
) -> std::sync::Arc<orca_runtime::surface::SurfaceSnapshot> {
    match surface.attach_fresh(FreshAttachRequest {
        request_id: SurfaceRequestId::new(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::<SurfaceInteractionKind>::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment.baseline.snapshot,
        _ => panic!("fresh TUI attachment failed"),
    }
}

fn surface_operation_id_text(operation_id: &SurfaceOperationId) -> String {
    uuid::Uuid::from_bytes(*operation_id.as_bytes())
        .hyphenated()
        .to_string()
}

fn test_config(cwd: PathBuf, history_mode: HistoryMode) -> RunConfig {
    RunConfig {
        app_version: "test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::Suggest,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::parse(None).unwrap(),
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: HashMap::new(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        budget: Default::default(),
        subagents: SubagentConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::default(),
        vim_mode: false,
        vim_insert_escape: None,
        update_check: false,
        desktop_notifications: false,
        terminal_notifications: false,
        auto_memory: false,
    }
}
