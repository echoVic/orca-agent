use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::CancelToken;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::cost_types::UsageTotals as RecordedUsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_runtime::runtime_host::{
    GenerationContext, HostedTurnRequest, RuntimeHost, RuntimeHostError, RuntimeThreadHandle,
    ThreadOperationExecutor, ThreadOperationOutcome,
};
use orca_runtime::surface::*;
use orca_runtime::thread::RuntimeThread;
use orca_runtime::thread_store::{SessionStore, ThreadStore};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const RESTART_CHILD_ENV: &str = "ORCA_SURFACE_OPERATION_RESTART_CHILD";
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
struct ExecutionGate {
    inner: Arc<(Mutex<ExecutionGateState>, Condvar)>,
}

#[derive(Default)]
struct ExecutionGateState {
    entered: bool,
    cancel_seen: bool,
    released: bool,
}

impl ExecutionGate {
    fn new() -> Self {
        Self {
            inner: Arc::new((Mutex::new(ExecutionGateState::default()), Condvar::new())),
        }
    }

    fn enter(&self) {
        let (state, changed) = &*self.inner;
        let mut state = state.lock().unwrap();
        state.entered = true;
        changed.notify_all();
    }

    fn wait_until_entered(&self) {
        self.wait_until(|state| state.entered, "generation did not enter executor");
    }

    fn wait_until_cancel_seen(&self) {
        self.wait_until(
            |state| state.cancel_seen,
            "generation did not observe cancellation",
        );
    }

    fn has_entered(&self) -> bool {
        let (state, _) = &*self.inner;
        state.lock().unwrap().entered
    }

    fn wait_for_release(&self) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        let (state, changed) = &*self.inner;
        let mut state = state.lock().unwrap();
        while !state.released {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "generation was not released");
            let (next, timed_out) = changed.wait_timeout(state, remaining).unwrap();
            state = next;
            assert!(!timed_out.timed_out(), "generation was not released");
        }
    }

    fn wait_for_cancel_and_release(&self, cancel: &CancelToken) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !cancel.is_cancelled() {
            assert!(Instant::now() < deadline, "generation was not cancelled");
            std::thread::sleep(Duration::from_millis(5));
        }
        {
            let (state, changed) = &*self.inner;
            let mut state = state.lock().unwrap();
            state.cancel_seen = true;
            changed.notify_all();
        }
        self.wait_for_release();
    }

    fn release(&self) {
        let (state, changed) = &*self.inner;
        let mut state = state.lock().unwrap();
        state.released = true;
        changed.notify_all();
    }

    fn wait_until(&self, predicate: impl Fn(&ExecutionGateState) -> bool, message: &str) {
        let deadline = Instant::now() + TEST_TIMEOUT;
        let (state, changed) = &*self.inner;
        let mut state = state.lock().unwrap();
        while !predicate(&state) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "{message}");
            let (next, timed_out) = changed.wait_timeout(state, remaining).unwrap();
            state = next;
            assert!(!timed_out.timed_out(), "{message}");
        }
    }
}

struct ExecutionReleaseGuard(ExecutionGate);

impl Drop for ExecutionReleaseGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

enum ExecutionBehavior {
    HoldSuccess(ExecutionGate),
    HoldSuccessAfterPermit {
        permit: ExecutionGate,
        gate: ExecutionGate,
    },
    HoldUntilCancelled(ExecutionGate),
    PanicIfCalled,
}

struct ScriptedExecutor {
    behaviors: Mutex<VecDeque<ExecutionBehavior>>,
    calls: AtomicUsize,
}

impl ScriptedExecutor {
    fn new(behaviors: impl IntoIterator<Item = ExecutionBehavior>) -> Self {
        Self {
            behaviors: Mutex::new(behaviors.into_iter().collect()),
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

impl ThreadOperationExecutor for ScriptedExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        _request: &HostedTurnRequest,
        _generation: &GenerationContext,
        _events: &mut EventFactory,
        _writer: &mut (dyn Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<ThreadOperationOutcome> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let behavior = self
            .behaviors
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted execution behavior");
        let status = match behavior {
            ExecutionBehavior::HoldSuccess(gate) => {
                gate.enter();
                gate.wait_for_release();
                RunStatus::Success
            }
            ExecutionBehavior::HoldSuccessAfterPermit { permit, gate } => {
                permit.wait_for_release();
                gate.enter();
                gate.wait_for_release();
                RunStatus::Success
            }
            ExecutionBehavior::HoldUntilCancelled(gate) => {
                gate.enter();
                gate.wait_for_cancel_and_release(cancel);
                RunStatus::Cancelled
            }
            ExecutionBehavior::PanicIfCalled => {
                panic!("operation must not reach the executor")
            }
        };
        thread.lifecycle_mut().finish_task(status);
        Ok(status.into())
    }
}

struct ForegroundHarness {
    _cwd: tempfile::TempDir,
    host: Option<RuntimeHost>,
    thread: RuntimeThreadHandle,
    surface: RuntimeSurfaceHandle,
    client: RuntimeSurfaceClientHandle,
    subscription: SurfaceSubscriptionReceiver,
    baseline: Arc<SurfaceSnapshot>,
    executor: Arc<ScriptedExecutor>,
}

impl ForegroundHarness {
    fn recorded(behavior: ExecutionBehavior) -> Self {
        let cwd = tempfile::tempdir().unwrap();
        let executor = Arc::new(ScriptedExecutor::new([behavior]));
        let host = RuntimeHost::start_with_executor(executor.clone()).expect("start runtime host");
        let thread = host
            .start_thread(
                test_config(cwd.path().to_path_buf(), HistoryMode::Record),
                "surface operation test",
            )
            .expect("start runtime thread");
        let surface = thread.surface();
        let attachment = fresh_attachment(&surface);
        let subscription = surface
            .claim_subscription(&attachment.subscription)
            .expect("claim surface subscription once");
        Self {
            _cwd: cwd,
            host: Some(host),
            thread,
            surface,
            client: attachment.client,
            subscription,
            baseline: attachment.baseline.snapshot,
            executor,
        }
    }

    fn reserve(&mut self, text: &str) -> ReservedOperationOutput {
        let intent = user_turn_intent(&self.baseline, text);
        committed_value(
            self.client
                .reserve_operation(request_id(), intent)
                .expect("reserve foreground operation"),
        )
    }

    fn admit(&mut self, reserved: &ReservedOperationOutput) -> SurfaceOperationFence {
        let output = committed_value(
            self.client
                .admit_reserved(
                    request_id(),
                    reserved.operation_id.clone(),
                    reserved.lease.lease_id.clone(),
                )
                .expect("admit foreground operation"),
        );
        match output {
            AdmissionOutput::Admitted {
                operation_id,
                first_generation,
                ..
            } => {
                assert_eq!(operation_id, reserved.operation_id);
                first_generation
            }
            AdmissionOutput::Queued { .. } => panic!("idle foreground operation was queued"),
        }
    }

    fn cancel(&self, operation_id: SurfaceOperationId) -> CancelOperationOutput {
        committed_value(
            self.client
                .cancel_operation(request_id(), operation_id)
                .expect("cancel foreground operation"),
        )
    }

    fn collect_until(
        &mut self,
        predicate: impl Fn(&[SurfaceCommitBatch]) -> bool,
    ) -> Vec<SurfaceCommitBatch> {
        collect_until(&mut self.subscription, predicate)
    }

    fn wait_terminal(&self, operation_id: SurfaceOperationId) -> OperationTerminalAtCursor {
        terminal_value(
            self.client
                .wait_operation_terminal(request_id(), operation_id)
                .expect("wait for operation terminal"),
        )
    }

    fn shutdown_host(&mut self) {
        if let Some(host) = self.host.take() {
            host.shutdown().expect("shutdown runtime host");
        }
    }
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
        max_budget_usd: None,
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

fn user_turn_intent(snapshot: &SurfaceSnapshot, text: &str) -> OperationRequestIntent {
    OperationRequestIntent {
        correlation: OperationIngressCorrelation::TuiUser,
        kind: OperationKind::UserTurn,
        input: Some(SurfaceInputRequest {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                text: DisplayText::new(text),
            }])
            .unwrap(),
        }),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: snapshot.settings.thread_revision,
            expected_policy_epoch: snapshot.settings.effective.policy_epoch,
        },
    }
}

fn fresh_attachment(surface: &RuntimeSurfaceHandle) -> FreshSurfaceAttachment {
    match surface.attach_fresh(FreshAttachRequest {
        request_id: request_id(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
            SurfaceCapability::ControlBoundOperation,
        ]),
        interaction_capabilities: BTreeSet::new(),
    }) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("fresh TUI attachment failed"),
    }
}

fn request_id() -> SurfaceRequestId {
    let value = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    SurfaceRequestId::try_from_bytes(uuid_v7_bytes(value)).unwrap()
}

fn uuid_v7_bytes(value: u64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&value.to_be_bytes());
    bytes[8..].copy_from_slice(&value.rotate_left(17).to_be_bytes());
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    bytes
}

fn committed_value<T>(reply: MutationReply<T>) -> T {
    match reply {
        MutationReply::Committed { value, .. } => value,
        MutationReply::Deferred { .. } => panic!("operation mutation was deferred"),
        MutationReply::Uncommitted { .. } => panic!("operation mutation was not committed"),
    }
}

fn terminal_value(result: WaitOperationTerminalResult) -> OperationTerminalAtCursor {
    match result {
        WaitOperationTerminalResult::Terminal { value } => value,
        WaitOperationTerminalResult::TerminalCommitFailure { .. } => {
            panic!("terminal commit requires repair")
        }
        WaitOperationTerminalResult::TerminalProjectionFailure { .. } => {
            panic!("terminal projection requires repair")
        }
        WaitOperationTerminalResult::UnknownOperation { .. } => panic!("unknown operation"),
        WaitOperationTerminalResult::WrongThread { .. } => {
            panic!("operation belongs to another thread")
        }
        WaitOperationTerminalResult::WaitCancelled { .. } => panic!("terminal wait was cancelled"),
    }
}

fn collect_until(
    receiver: &mut SurfaceSubscriptionReceiver,
    predicate: impl Fn(&[SurfaceCommitBatch]) -> bool,
) -> Vec<SurfaceCommitBatch> {
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut batches = Vec::new();
    loop {
        while let Some(item) = receiver.try_recv() {
            match item {
                SurfaceSubscriptionItem::Batch { batch } => {
                    batches.push(batch);
                    if predicate(&batches) {
                        return batches;
                    }
                }
                SurfaceSubscriptionItem::Gap { required } => {
                    panic!("surface subscription gapped: {:?}", required.reason)
                }
                SurfaceSubscriptionItem::Sealed { reason } => {
                    panic!("surface subscription sealed before predicate: {reason:?}")
                }
            }
        }
        if predicate(&batches) {
            return batches;
        }
        assert!(
            Instant::now() < deadline,
            "expected surface batch was not published"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn operation_owner_attachment_controls_admit_and_cancel() {
    let gate = ExecutionGate::new();
    let mut harness = ForegroundHarness::recorded(ExecutionBehavior::HoldSuccess(gate.clone()));
    let other = fresh_attachment(&harness.surface);
    let reserved = harness.reserve("owner-bound operation");

    assert!(matches!(
        other.client.admit_reserved(
            request_id(),
            reserved.operation_id.clone(),
            reserved.lease.lease_id.clone(),
        ),
        Err(SurfaceClientCommandError::Unauthorized)
    ));
    assert!(matches!(
        other
            .client
            .cancel_operation(request_id(), reserved.operation_id.clone()),
        Err(SurfaceClientCommandError::Unauthorized)
    ));

    harness.admit(&reserved);
    gate.wait_until_entered();
    assert!(matches!(
        other
            .client
            .cancel_operation(request_id(), reserved.operation_id.clone()),
        Err(SurfaceClientCommandError::Unauthorized)
    ));
    harness.cancel(reserved.operation_id.clone());
    gate.release();
    harness.wait_terminal(reserved.operation_id);
    harness.shutdown_host();
}

#[test]
fn use_current_settings_requires_exact_revision_before_requested() {
    let mut harness = ForegroundHarness::recorded(ExecutionBehavior::PanicIfCalled);
    let snapshot = harness.baseline.as_ref();
    let stale_revision = SettingsRevision::try_new(snapshot.settings.thread_revision.get() + 1)
        .expect("next settings revision is valid");
    let intent = OperationRequestIntent {
        correlation: OperationIngressCorrelation::TuiUser,
        kind: OperationKind::UserTurn,
        input: Some(SurfaceInputRequest {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                text: DisplayText::new("stale settings"),
            }])
            .unwrap(),
        }),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: stale_revision,
            expected_policy_epoch: snapshot.settings.effective.policy_epoch,
        },
    };

    let result = harness
        .client
        .reserve_operation(request_id(), intent)
        .expect("stale settings is a committed surface response");
    match result {
        MutationReply::Uncommitted {
            mutation: UncommittedMutation::Stale { error, .. },
        } => assert_eq!(
            error.error().code,
            SurfaceMutationErrorCode::StaleRevision,
            "settings CAS mismatch must be classified as stale"
        ),
        MutationReply::Committed { .. } => {
            panic!("stale settings unexpectedly requested operation")
        }
        MutationReply::Deferred { .. } => panic!("stale settings unexpectedly deferred"),
        MutationReply::Uncommitted { mutation } => {
            panic!("wrong stale mutation classification: {mutation:?}")
        }
    }
    harness.shutdown_host();
}

#[test]
fn unsupported_thread_overrides_are_rejected_before_requested() {
    let mut harness = ForegroundHarness::recorded(ExecutionBehavior::PanicIfCalled);
    let snapshot = harness.baseline.as_ref();
    let intent = OperationRequestIntent {
        correlation: OperationIngressCorrelation::TuiUser,
        kind: OperationKind::UserTurn,
        input: Some(SurfaceInputRequest {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
                text: DisplayText::new("unsupported settings"),
            }])
            .unwrap(),
        }),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested {
            expected_settings_revision: snapshot.settings.thread_revision,
            expected_policy_epoch: snapshot.settings.effective.policy_epoch,
            patches: NonEmptyVec::try_new(vec![RuntimeSettingsPatch::ApplyPermissionUpdate {
                update: SurfacePermissionUpdate::SetMode {
                    destination: SurfaceSettingsDestination::UserSettings,
                    mode: SurfaceApprovalMode::AutoEdit,
                },
            }])
            .unwrap(),
        },
    };

    assert_eq!(
        harness.client.reserve_operation(request_id(), intent).err(),
        Some(SurfaceClientCommandError::Unauthorized)
    );
    let snapshot = fresh_attachment(&harness.surface).baseline.snapshot;
    assert!(snapshot.foreground_operation.is_none());
    assert!(snapshot.queued_operations.is_empty());
    assert!(snapshot.operation_history.is_empty());
    harness.shutdown_host();
}

fn operation_patches<'a>(
    batches: &'a [SurfaceCommitBatch],
    operation_id: &SurfaceOperationId,
) -> Vec<(&'a SurfaceCommitBatch, &'a OperationPatch)> {
    let mut patches = Vec::new();
    for batch in batches {
        for envelope in batch.events.as_slice() {
            let SurfaceEvent::Operation(patch) = &envelope.event else {
                continue;
            };
            let matches_operation = match patch {
                OperationPatch::Requested { operation } => &operation.operation_id == operation_id,
                OperationPatch::ReservationQueueChanged {
                    operation_id: id, ..
                }
                | OperationPatch::Admitted {
                    operation_id: id, ..
                }
                | OperationPatch::ControlIntentCommitted {
                    operation_id: id, ..
                }
                | OperationPatch::Suspended {
                    operation_id: id, ..
                }
                | OperationPatch::SuspensionRebasedAfterUnstartedResume {
                    operation_id: id, ..
                }
                | OperationPatch::RecoveryRequired {
                    operation_id: id, ..
                }
                | OperationPatch::FinalizationStarted {
                    operation_id: id, ..
                }
                | OperationPatch::FinalizationSettlementRecorded {
                    operation_id: id, ..
                }
                | OperationPatch::FinalizationDegraded {
                    operation_id: id, ..
                } => id == operation_id,
                OperationPatch::InputBindingsResolved { fence, .. }
                | OperationPatch::InputBindingsFailed { fence, .. }
                | OperationPatch::GenerationStarted { fence, .. }
                | OperationPatch::AgentLoopTurnStarted {
                    turn: SurfaceAgentLoopTurn { fence, .. },
                }
                | OperationPatch::ModelRouteSelected { fence, .. }
                | OperationPatch::VerificationStarted { fence, .. }
                | OperationPatch::VerificationCompleted { fence, .. }
                | OperationPatch::GenerationStopped { fence, .. }
                | OperationPatch::GenerationTransferred { fence, .. } => {
                    &fence.operation_id == operation_id
                }
                OperationPatch::GenerationReserved { generation } => {
                    &generation.fence.operation_id == operation_id
                }
                OperationPatch::Terminal { record } => &record.operation_id == operation_id,
            };
            if matches_operation {
                patches.push((batch, patch));
            }
        }
    }
    patches
}

fn has_patch(
    batches: &[SurfaceCommitBatch],
    operation_id: &SurfaceOperationId,
    predicate: impl Fn(&OperationPatch) -> bool,
) -> bool {
    operation_patches(batches, operation_id)
        .into_iter()
        .any(|(_, patch)| predicate(patch))
}

fn assert_recorded_contiguous(batches: &[SurfaceCommitBatch]) {
    for batch in batches {
        assert!(matches!(batch.commit_class, CommitClass::Recorded { .. }));
    }
    for pair in batches.windows(2) {
        assert_eq!(pair[0].cursor_after, pair[1].cursor_before);
    }
}

fn operation_in_snapshot<'a>(
    snapshot: &'a SurfaceSnapshot,
    operation_id: &SurfaceOperationId,
) -> &'a OperationRecord {
    snapshot
        .foreground_operation
        .iter()
        .chain(snapshot.queued_operations.iter())
        .chain(snapshot.operation_history.iter())
        .find(|operation| &operation.operation_id == operation_id)
        .expect("operation is visible in snapshot")
}

fn assert_one_finalizer_and_terminal<'a>(
    batches: &'a [SurfaceCommitBatch],
    operation_id: &SurfaceOperationId,
) -> (
    SurfaceFinalizeIntentId,
    SurfaceCommitId,
    &'a SurfaceCommitBatch,
) {
    let patches = operation_patches(batches, operation_id);
    let finalizers = patches
        .iter()
        .filter_map(|(_, patch)| match patch {
            OperationPatch::FinalizationStarted {
                finalize_intent_id,
                terminal_commit_id,
                ..
            } => Some((finalize_intent_id.clone(), terminal_commit_id.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let terminals = patches
        .iter()
        .filter_map(|(batch, patch)| match patch {
            OperationPatch::Terminal { record } => Some((*batch, record)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finalizers.len(), 1, "operation must have one finalizer");
    assert_eq!(terminals.len(), 1, "operation must have one terminal batch");
    assert_eq!(terminals[0].1.finalize_intent_id, finalizers[0].0);
    match &terminals[0].0.commit_class {
        CommitClass::Recorded { commit_id, .. } => assert_eq!(commit_id, &finalizers[0].1),
        CommitClass::Ephemeral { .. } => panic!("recorded operation used ephemeral terminal"),
    }
    (
        finalizers[0].0.clone(),
        finalizers[0].1.clone(),
        terminals[0].0,
    )
}

fn assert_generation_stop_and_finalizer_are_atomic(
    batches: &[SurfaceCommitBatch],
    operation_id: &SurfaceOperationId,
) -> (
    SurfaceOperationFence,
    SurfaceFinalizeIntentId,
    SurfaceCommitId,
) {
    let matching = batches
        .iter()
        .filter_map(|batch| {
            let stop = batch
                .events
                .as_slice()
                .iter()
                .find_map(|envelope| match &envelope.event {
                    SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                        fence, ..
                    }) if &fence.operation_id == operation_id => Some(fence),
                    _ => None,
                });
            let finalizer =
                batch
                    .events
                    .as_slice()
                    .iter()
                    .find_map(|envelope| match &envelope.event {
                        SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                            operation_id: patch_operation_id,
                            finalize_intent_id,
                            terminal_commit_id,
                            ..
                        }) if patch_operation_id == operation_id => {
                            Some((finalize_intent_id, terminal_commit_id))
                        }
                        _ => None,
                    });
            stop.zip(finalizer)
                .map(|(fence, finalizer)| (batch, fence, finalizer))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "one durable batch must atomically pair the generation stop with its selected finalizer"
    );

    let (_, fence, (finalize_intent_id, terminal_commit_id)) = matching[0];
    (
        (*fence).clone(),
        (*finalize_intent_id).clone(),
        (*terminal_commit_id).clone(),
    )
}

fn with_orca_home<T>(body: impl FnOnce(&Path) -> T) -> T {
    let _guard = ORCA_HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("ORCA_HOME");
    // SAFETY: this integration test is run with one test thread and serialized by the lock.
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(home.path())));
    // SAFETY: the same process-wide lock still guards restoration.
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("ORCA_HOME", previous);
        } else {
            std::env::remove_var("ORCA_HOME");
        }
    }
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[cfg(unix)]
#[test]
fn duplicate_surface_owner_rejects_resume_before_transcript_append_or_materialization() {
    with_orca_home(|_| {
        let cwd = tempfile::tempdir().unwrap();
        let store = SessionStore::default();
        let mut seed = store
            .create_live_thread(cwd.path(), "mock", None, "duplicate owner fixture")
            .expect("create seed transcript");
        seed.writer_mut()
            .append_usage(RecordedUsageTotals {
                input_tokens: 1,
                output_tokens: 1,
                cache_tokens: 0,
                estimated_cost_usd: 0.0,
            })
            .expect("seed usage so an erroneous resume append changes transcript bytes");
        let thread_id = seed.thread_id().to_string();
        drop(seed);

        let transcript_path = store
            .load_session(&thread_id)
            .expect("load seed transcript")
            .path;
        let owner_executor = Arc::new(ScriptedExecutor::new(Vec::<ExecutionBehavior>::new()));
        let owner_host =
            RuntimeHost::start_with_executor(owner_executor).expect("start owner runtime host");
        let owner_thread = owner_host
            .start_thread(
                test_config(
                    cwd.path().to_path_buf(),
                    HistoryMode::Resume(thread_id.clone()),
                ),
                "active transcript owner",
            )
            .expect("resume transcript as active owner");
        assert_eq!(owner_thread.thread_id(), thread_id);

        let epoch_path = transcript_path.with_extension("surface-owner.epoch");
        let transcript_before = fs::read(&transcript_path).expect("read owned transcript baseline");
        let epoch_before = fs::read(&epoch_path).expect("read owned epoch baseline");

        let contender_executor = Arc::new(ScriptedExecutor::new(Vec::<ExecutionBehavior>::new()));
        let contender_host = RuntimeHost::start_with_executor(contender_executor)
            .expect("start contender runtime host");
        let error = contender_host
            .start_thread(
                test_config(
                    cwd.path().to_path_buf(),
                    HistoryMode::Resume(thread_id.clone()),
                ),
                "duplicate transcript owner",
            )
            .expect_err("duplicate owner must fail before runtime materialization");
        match error {
            RuntimeHostError::ThreadStartFailed { message } => assert_eq!(
                message, "failed to acquire typed surface owner lease: AlreadyOwned",
                "duplicate owner must fail closed at lease acquisition"
            ),
            other => panic!("unexpected duplicate owner failure: {other:?}"),
        }

        assert_eq!(
            fs::read(&transcript_path).expect("reread transcript after rejected contender"),
            transcript_before,
            "rejected owner appended or materialized transcript records"
        );
        assert_eq!(
            fs::read(&epoch_path).expect("reread epoch after rejected contender"),
            epoch_before,
            "rejected owner advanced the durable owner epoch"
        );

        contender_host.shutdown().expect("shutdown contender host");
        owner_host.shutdown().expect("shutdown owner host");
    });
}

#[test]
fn tui_foreground_reserve_admit_starts_exact_fence_before_execution() {
    with_orca_home(|_| {
        const INPUT_TEXT: &str = "inspect the repository";
        let gate = ExecutionGate::new();
        let entry_permit = ExecutionGate::new();
        let mut harness = ForegroundHarness::recorded(ExecutionBehavior::HoldSuccessAfterPermit {
            permit: entry_permit.clone(),
            gate: gate.clone(),
        });
        let _release_guard = ExecutionReleaseGuard(gate.clone());
        let _entry_permit_guard = ExecutionReleaseGuard(entry_permit.clone());
        let reserved = harness.reserve(INPUT_TEXT);
        let requested = harness.collect_until(|batches| {
            has_patch(batches, &reserved.operation_id, |patch| {
                matches!(patch, OperationPatch::Requested { .. })
            })
        });
        assert_recorded_contiguous(&requested);
        assert!(!has_patch(&requested, &reserved.operation_id, |patch| {
            matches!(patch, OperationPatch::Admitted { .. })
        }));
        let expected_request_digest = operation_patches(&requested, &reserved.operation_id)
            .into_iter()
            .find_map(|(_, patch)| match patch {
                OperationPatch::Requested { operation } => {
                    match &operation.intent.initial_replayability {
                        Replayability::Replayable {
                            request: Some(request),
                            request_digest: Some(request_digest),
                            ..
                        } => {
                            assert!(matches!(
                                request.blocks.as_slice(),
                                [SurfaceInputRequestBlock::Text { text }]
                                    if text == &DisplayText::new(INPUT_TEXT)
                            ));
                            Some(request_digest.clone())
                        }
                        _ => panic!("recorded UserTurn must retain its replayable input request"),
                    }
                }
                _ => None,
            })
            .expect("requested operation fact");

        let admitted_fence = harness.admit(&reserved);
        let execution_facts = harness.collect_until(|batches| {
            has_patch(batches, &reserved.operation_id, |patch| {
                matches!(
                    patch,
                    OperationPatch::AgentLoopTurnStarted { turn }
                        if turn.fence == admitted_fence
                )
            })
        });
        assert_recorded_contiguous(&execution_facts);
        let (
            admission_batch,
            logical_turn_id,
            input_item_id,
            presentation,
            correlation_id,
            first_generation,
        ) = operation_patches(&execution_facts, &reserved.operation_id)
            .into_iter()
            .find_map(|(batch, patch)| match patch {
                OperationPatch::Admitted {
                    logical_turn_id,
                    input:
                        AdmittedInput::PendingUser {
                            item_id,
                            presentation,
                            correlation_id,
                        },
                    first_generation,
                    ..
                } => Some((
                    batch,
                    logical_turn_id,
                    item_id,
                    presentation,
                    correlation_id,
                    first_generation,
                )),
                OperationPatch::Admitted {
                    input: AdmittedInput::NotApplicable,
                    ..
                } => panic!("UserTurn admission must carry pending user input"),
                _ => None,
            })
            .expect("UserTurn admission fact");
        assert_eq!(&first_generation.fence, &admitted_fence);
        assert_eq!(
            &first_generation.input,
            &GenerationInputState::Pending {
                input_item_id: input_item_id.clone(),
                presentation: presentation.clone(),
                correlation_id: correlation_id.clone(),
            }
        );
        assert_eq!(
            presentation,
            &SurfaceInputPresentation::Visible {
                text: DisplayText::new(INPUT_TEXT),
            }
        );
        assert_eq!(
            admission_batch
                .events
                .as_slice()
                .iter()
                .filter(|envelope| matches!(
                    &envelope.event,
                    SurfaceEvent::Item(ItemPatch::Added {
                        item: SurfaceItem::UserMessage {
                            id,
                            turn_id,
                            input: SurfaceUserInputState::Pending {
                                presentation: item_presentation,
                                correlation_id: item_correlation,
                            },
                            pinned: false,
                            origin: SurfaceItemOrigin::UserInput,
                        }
                    }) if id == input_item_id
                        && turn_id == logical_turn_id
                        && item_presentation == presentation
                        && item_correlation == correlation_id
                ))
                .count(),
            1,
            "admission must atomically add one exact pending user item"
        );
        let generation_started_batch = operation_patches(&execution_facts, &reserved.operation_id)
            .into_iter()
            .find_map(|(batch, patch)| match patch {
                OperationPatch::GenerationStarted { fence, .. } if fence == &admitted_fence => {
                    Some(batch)
                }
                _ => None,
            })
            .expect("generation start fact");

        let (resolution_batch, resolution_fact) =
            operation_patches(&execution_facts, &reserved.operation_id)
                .into_iter()
                .find_map(|(batch, patch)| match patch {
                    OperationPatch::InputBindingsResolved {
                        fence,
                        input_item_id: resolved_item_id,
                        fact,
                    } if fence == &admitted_fence && resolved_item_id == input_item_id => {
                        Some((batch, fact))
                    }
                    _ => None,
                })
                .expect("started UserTurn must resolve its exact input item");
        assert_eq!(
            generation_started_batch.cursor_after, resolution_batch.cursor_before,
            "input resolution must follow GenerationStarted without a cursor gap"
        );
        assert_eq!(
            resolution_batch
                .events
                .as_slice()
                .iter()
                .filter(|envelope| matches!(
                    &envelope.event,
                    SurfaceEvent::Item(ItemPatch::InputResolved { item_id, fact })
                        if item_id == input_item_id && fact == resolution_fact
                ))
                .count(),
            1,
            "input resolution must atomically update the exact pending item"
        );
        match resolution_fact {
            SurfaceResolvedInputFact::Replayable {
                input,
                request_digest,
            } => {
                assert_eq!(request_digest, &expected_request_digest);
                assert_eq!(input.canonical_text, DisplayText::new(INPUT_TEXT));
                assert!(matches!(
                    input.blocks.as_slice(),
                    [SurfaceInputBlock::Text { text }]
                        if text == &DisplayText::new(INPUT_TEXT)
                ));
            }
            SurfaceResolvedInputFact::NonReplayable { .. } => {
                panic!("recorded UserTurn must resolve to a replayable input fact")
            }
        }

        let (agent_loop_started_batch, agent_loop_turn) =
            operation_patches(&execution_facts, &reserved.operation_id)
                .into_iter()
                .find_map(|(batch, patch)| match patch {
                    OperationPatch::AgentLoopTurnStarted { turn } => Some((batch, turn)),
                    _ => None,
                })
                .expect("started generation must durably start its first agent-loop turn");
        assert_eq!(
            resolution_batch.cursor_after, agent_loop_started_batch.cursor_before,
            "AgentLoopTurnStarted must follow input resolution without a cursor gap"
        );
        assert_eq!(&agent_loop_turn.fence, &admitted_fence);
        assert_eq!(&agent_loop_turn.turn_id, logical_turn_id);
        assert_eq!(agent_loop_turn.ordinal, 0);
        assert!(
            !gate.has_entered(),
            "agent-loop turn start must be durable before executor gate entry"
        );

        entry_permit.release();
        gate.wait_until_entered();

        let live = fresh_attachment(&harness.surface).baseline.snapshot;
        let operation = operation_in_snapshot(&live, &reserved.operation_id);
        assert!(matches!(operation.phase, OperationPhase::Admitted));
        assert!(matches!(
            operation.generations.last(),
            Some(GenerationRecord {
                fence,
                phase: GenerationPhase::Started,
                started_witness: Some(_),
                ..
            }) if fence == &admitted_fence
        ));

        gate.release();
        let terminal = harness.wait_terminal(reserved.operation_id);
        assert!(matches!(
            terminal.terminal,
            OperationTerminal::Succeeded { .. }
        ));
        harness.shutdown_host();
    });
}

#[test]
fn tui_foreground_success_commits_one_finalizer_and_terminal_cursor() {
    with_orca_home(|_| {
        let gate = ExecutionGate::new();
        let mut harness = ForegroundHarness::recorded(ExecutionBehavior::HoldSuccess(gate.clone()));
        let reserved = harness.reserve("finish successfully");
        let operation_id = reserved.operation_id.clone();
        harness.admit(&reserved);
        gate.wait_until_entered();

        let wait_client = harness.client.clone();
        let wait_operation_id = operation_id.clone();
        let (wait_tx, wait_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = wait_client.wait_operation_terminal(request_id(), wait_operation_id);
            let _ = wait_tx.send(result);
        });
        gate.release();

        let batches = harness.collect_until(|batches| {
            has_patch(batches, &operation_id, |patch| {
                matches!(patch, OperationPatch::Terminal { .. })
            })
        });
        let waited = terminal_value(
            wait_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("terminal waiter did not wake")
                .expect("terminal waiter failed"),
        );
        let (finalize_intent_id, terminal_commit_id, terminal_batch) =
            assert_one_finalizer_and_terminal(&batches, &operation_id);
        let (stopped_fence, atomic_finalize_intent_id, atomic_terminal_commit_id) =
            assert_generation_stop_and_finalizer_are_atomic(&batches, &operation_id);
        assert_eq!(atomic_finalize_intent_id, finalize_intent_id);
        assert_eq!(atomic_terminal_commit_id, terminal_commit_id);
        let terminal_record = operation_patches(&batches, &operation_id)
            .into_iter()
            .find_map(|(_, patch)| match patch {
                OperationPatch::Terminal { record } => Some(record),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            terminal_record.terminal,
            OperationTerminal::Succeeded { .. }
        ));
        assert_eq!(waited.operation_id, operation_id);
        assert_eq!(waited.terminal, terminal_record.terminal);
        assert_eq!(waited.cursor, terminal_batch.cursor_after);
        assert_eq!(waited.commit_class, terminal_batch.commit_class);
        assert_eq!(waited.batch_digest, terminal_batch.batch_digest);

        let live = fresh_attachment(&harness.surface).baseline.snapshot;
        let operation = operation_in_snapshot(&live, &operation_id);
        let finalization = operation
            .finalization
            .as_ref()
            .expect("atomic stop batch must persist finalizer identity in reducer state");
        assert_eq!(finalization.finalize_intent_id, finalize_intent_id);
        assert_eq!(finalization.terminal_commit_id, terminal_commit_id);
        assert!(matches!(
            operation.generations.last(),
            Some(GenerationRecord {
                fence,
                phase: GenerationPhase::Stopped,
                ..
            }) if fence == &stopped_fence
        ));

        let replayed = harness.wait_terminal(waited.operation_id.clone());
        assert_eq!(replayed, waited);
        harness.shutdown_host();
    });
}

fn assert_unsupported_tui_intent_rejected_before_requested(
    case: &str,
    make_intent: impl FnOnce(OperationRequestIntent) -> OperationRequestIntent,
) {
    with_orca_home(|_| {
        let mut harness = ForegroundHarness::recorded(ExecutionBehavior::PanicIfCalled);
        let ledger_path = SessionStore::default()
            .load_session(harness.thread.thread_id())
            .expect("load foreground transcript")
            .path;
        let intent = make_intent(user_turn_intent(
            &harness.baseline,
            "unsupported intent fixture",
        ));
        let before = fresh_attachment(&harness.surface).baseline;
        let ledger_before = fs::read(&ledger_path).expect("read surface ledger baseline");
        let error = match harness.client.reserve_operation(request_id(), intent) {
            Err(error) => error,
            Ok(_) => panic!("{case} intent unexpectedly reserved an operation"),
        };
        assert_eq!(
            error,
            SurfaceClientCommandError::RuntimeUnavailable,
            "{case}"
        );

        let after = fresh_attachment(&harness.surface).baseline;
        assert_eq!(after.cursor, before.cursor, "{case} advanced the cursor");
        assert!(
            after.snapshot.as_ref() == before.snapshot.as_ref(),
            "{case} changed the reducer snapshot"
        );
        assert!(
            harness.subscription.try_recv().is_none(),
            "{case} published a subscription item"
        );
        assert_eq!(
            fs::read(&ledger_path).expect("reread surface ledger"),
            ledger_before,
            "{case} changed the durable ledger"
        );
        assert!(
            after
                .snapshot
                .foreground_operation
                .iter()
                .chain(after.snapshot.queued_operations.iter())
                .chain(after.snapshot.operation_history.iter())
                .all(|operation| !matches!(operation.phase, OperationPhase::Requested)),
            "{case} left a Requested operation"
        );

        assert_eq!(harness.executor.call_count(), 0);
        harness.shutdown_host();
    });
}

#[test]
fn tui_non_replayable_intent_is_rejected_before_requested() {
    assert_unsupported_tui_intent_rejected_before_requested("non-replayable", |mut intent| {
        intent.replayability = ReplayabilityRequest::NonReplayable {
            reason: NonReplayableReason::HistoryDisabled,
        };
        intent
    });
}

#[test]
fn tui_missing_input_is_rejected_before_requested() {
    assert_unsupported_tui_intent_rejected_before_requested("missing input", |mut intent| {
        intent.input = None;
        intent
    });
}

#[test]
fn tui_legacy_jsonl_mention_is_rejected_before_requested() {
    assert_unsupported_tui_intent_rejected_before_requested(
        "legacy JSONL mention",
        |mut intent| {
            intent.input = Some(SurfaceInputRequest {
                blocks: NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Binding {
                    binding: SurfaceInputBindingRequest::LegacyJsonlMention {
                        name: DisplayText::new("legacy-file"),
                        visible: DisplayText::new("@legacy-file"),
                        start: ByteOffset::new(0),
                        end: ByteOffset::new(12),
                        target: SurfaceLegacyMentionTarget::Skill {
                            id: DisplayText::new("legacy-file"),
                            path: SurfaceLegacyPath(DisplayText::new("/tmp/legacy-file")),
                        },
                    },
                }])
                .unwrap(),
            });
            intent
        },
    );
}

#[test]
fn tui_pre_admission_cancel_wakes_registered_terminal_waiter() {
    with_orca_home(|_| {
        let mut harness = ForegroundHarness::recorded(ExecutionBehavior::PanicIfCalled);
        let reserved = harness.reserve("cancel while terminal waiter is registered");
        let operation_id = reserved.operation_id.clone();

        let wait_client = harness.client.clone();
        let wait_operation_id = operation_id.clone();
        let (wait_tx, wait_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = wait_client.wait_operation_terminal(request_id(), wait_operation_id);
            let _ = wait_tx.send(result);
        });
        // The dispatcher enqueues before blocking. Let the actor store the waiter
        // before the cancellation command is submitted.
        std::thread::sleep(Duration::from_millis(50));

        let cancelled = harness.cancel(operation_id.clone());
        let CancelOperationOutput::CancelledBeforeAdmission { terminal } = cancelled else {
            panic!("reserved operation was not cancelled synchronously")
        };
        let waited = terminal_value(
            wait_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("registered pre-admission waiter did not wake")
                .expect("registered pre-admission waiter failed"),
        );
        assert_eq!(
            waited, terminal,
            "waiter and cancellation reply must expose the exact same terminal witness"
        );

        let batches = harness.collect_until(|batches| {
            has_patch(batches, &operation_id, |patch| {
                matches!(patch, OperationPatch::Terminal { .. })
            })
        });
        assert_one_finalizer_and_terminal(&batches, &operation_id);
        assert_eq!(harness.executor.call_count(), 0);
        harness.shutdown_host();
    });
}

#[test]
fn tui_terminal_wait_cancellation_retires_only_the_caller_waiter() {
    with_orca_home(|_| {
        let mut harness = ForegroundHarness::recorded(ExecutionBehavior::PanicIfCalled);
        let reserved = harness.reserve("cancel only terminal waiter");
        let operation_id = reserved.operation_id.clone();
        let cancellation = OptionalProcessLocalCancel::new();

        let wait_client = harness.client.clone();
        let wait_operation_id = operation_id.clone();
        let wait_cancellation = cancellation.clone();
        let (wait_tx, wait_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = wait_client.wait_operation_terminal_with_cancel(
                request_id(),
                wait_operation_id,
                wait_cancellation,
            );
            let _ = wait_tx.send(result);
        });

        // Give the actor time to register the waiter before canceling its caller-owned signal.
        std::thread::sleep(Duration::from_millis(50));
        cancellation.cancel();
        let waited = wait_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("canceled terminal waiter did not wake")
            .expect("canceled terminal waiter failed");
        assert!(matches!(
            waited,
            WaitOperationTerminalResult::WaitCancelled { operation_id: id }
                if id == operation_id
        ));

        let terminal = match harness.cancel(operation_id.clone()) {
            CancelOperationOutput::CancelledBeforeAdmission { terminal } => terminal,
            _ => panic!("caller-only wait cancellation changed operation state"),
        };
        assert_eq!(terminal.operation_id, operation_id);
        assert_eq!(harness.executor.call_count(), 0);
        harness.shutdown_host();
    });
}

#[test]
fn tui_requested_operation_is_durably_terminalized_before_shutdown_returns() {
    #[derive(Clone, Copy)]
    enum ShutdownMode {
        ThreadClose,
        HostShutdown,
    }

    with_orca_home(|_| {
        for mode in [ShutdownMode::ThreadClose, ShutdownMode::HostShutdown] {
            let mut harness = ForegroundHarness::recorded(ExecutionBehavior::PanicIfCalled);
            let reserved = harness.reserve("terminalize requested operation during shutdown");
            let operation_id = reserved.operation_id.clone();

            let wait_client = harness.client.clone();
            let wait_operation_id = operation_id.clone();
            let (wait_tx, wait_rx) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let result = wait_client.wait_operation_terminal(request_id(), wait_operation_id);
                let _ = wait_tx.send(result);
            });
            std::thread::sleep(Duration::from_millis(50));

            let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
            match mode {
                ShutdownMode::ThreadClose => {
                    let thread = harness.thread.clone();
                    std::thread::spawn(move || {
                        let _ = shutdown_tx.send(thread.shutdown());
                    });
                }
                ShutdownMode::HostShutdown => {
                    let host = harness.host.take().expect("owned runtime host");
                    std::thread::spawn(move || {
                        let _ = shutdown_tx.send(host.shutdown());
                    });
                }
            }

            shutdown_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("shutdown did not return")
                .expect("shutdown failed");
            let terminal = terminal_value(
                wait_rx
                    .recv_timeout(TEST_TIMEOUT)
                    .expect("requested-operation waiter did not wake before shutdown returned")
                    .expect("requested-operation waiter failed during shutdown"),
            );
            let expected_reason = match mode {
                ShutdownMode::ThreadClose => NotAdmittedReason::ThreadClose,
                ShutdownMode::HostShutdown => NotAdmittedReason::HostShutdown,
            };
            assert_eq!(terminal.operation_id, operation_id);
            assert_eq!(
                terminal.terminal,
                OperationTerminal::NotAdmitted {
                    reason: expected_reason,
                }
            );
            assert!(
                matches!(terminal.commit_class, CommitClass::Recorded { .. }),
                "requested shutdown terminal must be durable"
            );
            assert_eq!(harness.executor.call_count(), 0);

            if matches!(mode, ShutdownMode::ThreadClose) {
                harness.shutdown_host();
            }
        }
    });
}

#[test]
fn tui_foreground_cancel_is_serialized_across_admission_start() {
    with_orca_home(|_| {
        let mut before = ForegroundHarness::recorded(ExecutionBehavior::PanicIfCalled);
        let reserved = before.reserve("cancel before admission");
        let operation_id = reserved.operation_id.clone();
        let cancelled = before.cancel(operation_id.clone());
        let CancelOperationOutput::CancelledBeforeAdmission { terminal } = cancelled else {
            panic!("reserved operation was not cancelled synchronously")
        };
        assert!(matches!(
            terminal.terminal,
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::CancelledBeforeAdmission
            }
        ));
        let batches = before.collect_until(|batches| {
            has_patch(batches, &operation_id, |patch| {
                matches!(patch, OperationPatch::Terminal { .. })
            })
        });
        assert!(
            !operation_patches(&batches, &operation_id)
                .iter()
                .any(|(_, patch)| matches!(
                    patch,
                    OperationPatch::Admitted { .. }
                        | OperationPatch::GenerationStarted { .. }
                        | OperationPatch::GenerationStopped { .. }
                ))
        );
        assert_one_finalizer_and_terminal(&batches, &operation_id);
        assert_eq!(before.executor.call_count(), 0);
        assert_eq!(before.wait_terminal(operation_id), terminal);
        before.shutdown_host();

        let gate = ExecutionGate::new();
        let mut after =
            ForegroundHarness::recorded(ExecutionBehavior::HoldUntilCancelled(gate.clone()));
        let reserved = after.reserve("cancel after start");
        let operation_id = reserved.operation_id.clone();
        let fence = after.admit(&reserved);
        gate.wait_until_entered();
        let accepted = after.cancel(operation_id.clone());
        assert!(matches!(accepted, CancelOperationOutput::Accepted { .. }));
        gate.wait_until_cancel_seen();
        let control = after.collect_until(|batches| {
            has_patch(batches, &operation_id, |patch| {
                matches!(
                    patch,
                    OperationPatch::ControlIntentCommitted {
                        intent: PendingControlIntent::Terminalize {
                            cause: TerminalizationCause::UserCancel,
                            ..
                        },
                        ..
                    }
                )
            })
        });
        assert!(
            has_patch(&control, &operation_id, |patch| {
                matches!(
                    patch,
                    OperationPatch::GenerationStarted { fence: started, .. } if started == &fence
                )
            }) || operation_in_snapshot(
                &fresh_attachment(&after.surface).baseline.snapshot,
                &operation_id,
            )
            .generations
            .iter()
            .any(|generation| generation.fence == fence)
        );
        gate.release();
        let terminal = after.wait_terminal(operation_id.clone());
        assert!(matches!(
            terminal.terminal,
            OperationTerminal::Cancelled {
                reason: CancelReason::User
            }
        ));
        let terminal_batches = after.collect_until(|batches| {
            has_patch(batches, &operation_id, |patch| {
                matches!(patch, OperationPatch::Terminal { .. })
            })
        });
        assert_one_finalizer_and_terminal(&terminal_batches, &operation_id);
        after.shutdown_host();
    });
}

#[test]
fn tui_foreground_restart_reconciles_requested_and_started_states() {
    if let Ok(phase) = std::env::var(RESTART_CHILD_ENV) {
        run_restart_child(&phase);
    }

    with_orca_home(|home| {
        for phase in ["requested", "started"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("tui_foreground_restart_reconciles_requested_and_started_states")
                .arg("--nocapture")
                .arg("--test-threads=1")
                .env(RESTART_CHILD_ENV, phase)
                .env("ORCA_HOME", home)
                .status()
                .expect("start abrupt-owner-loss fixture");
            assert!(status.success(), "restart child failed for {phase}");

            let (thread_id, operation_id): (String, SurfaceOperationId) = serde_json::from_slice(
                &fs::read(restart_record_path(home, phase)).expect("read restart fixture identity"),
            )
            .unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let executor = Arc::new(ScriptedExecutor::new([ExecutionBehavior::PanicIfCalled]));
            let host = RuntimeHost::start_with_executor(executor.clone()).expect("restart host");
            let thread = host
                .start_thread(
                    test_config(
                        cwd.path().to_path_buf(),
                        HistoryMode::Resume(thread_id.clone()),
                    ),
                    "recover surface operation",
                )
                .expect("resume recorded thread and reconcile operations");
            assert_eq!(thread.thread_id(), thread_id);
            let surface = thread.surface();
            let attachment = fresh_attachment(&surface);
            let terminal = terminal_value(
                attachment
                    .client
                    .wait_operation_terminal(request_id(), operation_id.clone())
                    .expect("wait for recovered terminal"),
            );
            match phase {
                "requested" => assert!(matches!(
                    terminal.terminal,
                    OperationTerminal::NotAdmitted {
                        reason: NotAdmittedReason::RuntimeRestart
                    }
                )),
                "started" => assert!(matches!(
                    terminal.terminal,
                    OperationTerminal::AbortedByRuntimeRestart { .. }
                )),
                _ => unreachable!(),
            }
            let recovered = operation_in_snapshot(&attachment.baseline.snapshot, &operation_id);
            assert!(matches!(recovered.phase, OperationPhase::Terminal));
            assert_eq!(
                recovered.terminal.as_ref().unwrap().terminal,
                terminal.terminal
            );
            assert_eq!(executor.call_count(), 0);
            let repeated_cancel = committed_value(
                attachment
                    .client
                    .cancel_operation(request_id(), operation_id.clone())
                    .expect("terminal cancel is idempotent after recovery"),
            );
            assert!(matches!(
                repeated_cancel,
                CancelOperationOutput::AlreadyTerminal { terminal: replayed }
                    if replayed == terminal
            ));
            host.shutdown().expect("shutdown recovered host");
        }
    });
}

fn run_restart_child(phase: &str) -> ! {
    let behavior = match phase {
        "requested" => ExecutionBehavior::PanicIfCalled,
        "started" => ExecutionBehavior::HoldSuccess(ExecutionGate::new()),
        _ => panic!("unknown restart fixture phase: {phase}"),
    };
    let mut harness = ForegroundHarness::recorded(behavior);
    let reserved = harness.reserve("survive abrupt runtime restart");
    if phase == "started" {
        harness.admit(&reserved);
        let batches = harness.collect_until(|batches| {
            has_patch(batches, &reserved.operation_id, |patch| {
                matches!(patch, OperationPatch::GenerationStarted { .. })
            })
        });
        assert_recorded_contiguous(&batches);
    }
    let home = PathBuf::from(std::env::var_os("ORCA_HOME").expect("restart child ORCA_HOME"));
    fs::write(
        restart_record_path(&home, phase),
        serde_json::to_vec(&(
            harness.thread.thread_id().to_string(),
            reserved.operation_id,
        ))
        .unwrap(),
    )
    .expect("write restart fixture identity");
    std::process::exit(0)
}

fn restart_record_path(home: &Path, phase: &str) -> PathBuf {
    home.join(format!("runtime-surface-restart-{phase}.json"))
}

#[test]
fn foreground_shutdown_barrier_closes_only_after_terminal_witness() {
    #[derive(Clone, Copy)]
    enum ShutdownMode {
        ThreadClose,
        HostShutdown,
    }

    with_orca_home(|_| {
        for mode in [ShutdownMode::ThreadClose, ShutdownMode::HostShutdown] {
            let gate = ExecutionGate::new();
            let mut harness =
                ForegroundHarness::recorded(ExecutionBehavior::HoldUntilCancelled(gate.clone()));
            let reserved = harness.reserve("hold shutdown barrier");
            let operation_id = reserved.operation_id.clone();
            harness.admit(&reserved);
            gate.wait_until_entered();

            let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
            match mode {
                ShutdownMode::ThreadClose => {
                    let thread = harness.thread.clone();
                    std::thread::spawn(move || {
                        let _ = shutdown_tx.send(thread.shutdown());
                    });
                }
                ShutdownMode::HostShutdown => {
                    let host = harness.host.take().expect("owned runtime host");
                    std::thread::spawn(move || {
                        let _ = shutdown_tx.send(host.shutdown());
                    });
                }
            }

            gate.wait_until_cancel_seen();
            assert!(
                shutdown_rx.try_recv().is_err(),
                "shutdown returned before join"
            );
            let before_release = harness.collect_until(|batches| !batches.is_empty());
            assert!(!has_patch(&before_release, &operation_id, |patch| {
                matches!(patch, OperationPatch::Terminal { .. })
            }));

            gate.release();
            shutdown_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("shutdown did not settle")
                .expect("shutdown failed");
            let expected_reason = match mode {
                ShutdownMode::ThreadClose => SurfaceShutdownReason::ThreadClose,
                ShutdownMode::HostShutdown => SurfaceShutdownReason::HostShutdown,
            };
            let batches = harness.collect_until(|batches| {
                has_patch(batches, &operation_id, |patch| {
                    matches!(patch, OperationPatch::Terminal { .. })
                })
            });
            assert_recorded_contiguous(&batches);
            let (_, _, terminal_batch) = assert_one_finalizer_and_terminal(&batches, &operation_id);
            let (terminal_envelope, terminal_record) = terminal_batch
                .events
                .as_slice()
                .iter()
                .find_map(|envelope| match &envelope.event {
                    SurfaceEvent::Operation(OperationPatch::Terminal { record })
                        if record.operation_id == operation_id =>
                    {
                        Some((envelope, record))
                    }
                    _ => None,
                })
                .expect("terminal batch contains the operation terminal witness");
            assert!(matches!(
                terminal_record.terminal,
                OperationTerminal::Shutdown { reason } if reason == expected_reason
            ));
            assert_eq!(
                terminal_batch.cursor_after.next_seq.get(),
                terminal_batch.cursor_before.next_seq.get() + u64::from(terminal_batch.event_count)
            );
            assert_eq!(terminal_envelope.commit_class, terminal_batch.commit_class);
            assert_eq!(
                terminal_batch.batch_digest,
                canonical_batch_digest(terminal_batch)
            );

            if matches!(mode, ShutdownMode::ThreadClose) {
                harness.shutdown_host();
            }
        }
    });
}
