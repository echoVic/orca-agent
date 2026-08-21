use orca_runtime::surface::*;
use std::collections::BTreeSet;
use std::path::Path;
use std::thread;

fn uuid(seed: u8) -> [u8; 16] {
    let mut bytes = [seed; 16];
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    bytes
}

fn path() -> CanonicalPath {
    CanonicalPath::try_new(std::env::temp_dir().join("orca-surface-attach")).unwrap()
}

fn cursor(next_seq: u64) -> SurfaceCursor {
    SurfaceCursor {
        thread_id: SurfaceThreadId::try_from_bytes([1; 16]).unwrap(),
        incarnation: SurfaceIncarnation::try_from_bytes(uuid(2)).unwrap(),
        next_seq: SequenceNumber::new(next_seq),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(next_seq + 1).unwrap(),
        },
    }
}

fn snapshot(next_seq: u64) -> SurfaceSnapshot {
    let settings = SurfaceRuntimeSettings {
        model: NonEmptyText::try_new("deepseek-v4").unwrap(),
        reasoning_effort: SurfaceReasoningEffort::High,
        approval_mode: SurfaceApprovalMode::AutoEdit,
        cwd: path(),
        workspace_roots: vec![path()],
        active_permission_profile: None,
        permission_rules: SurfacePermissionRuleSet {
            ordered_rules: Vec::new(),
            digest: Sha256Digest::new([1; 32]),
        },
        additional_working_directories: Vec::new(),
        network_permissions: SurfaceNetworkPermissions {
            enabled: Some(true),
            domains: Vec::new(),
        },
        unsandboxed_shell: false,
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
    };
    SurfaceSnapshot {
        cursor: cursor(next_seq),
        thread: SurfaceThreadSnapshot {
            thread_id: cursor(0).thread_id,
            owner_epoch: ThreadOwnerEpoch::new(1),
            persistence: ThreadPersistence::RecordedCatalogued,
            title: DisplayText::new("attach test"),
            metadata_revision: SessionMetadataRevision::try_new(1).unwrap(),
            created_at: UnixMillis::new(1),
            updated_at: UnixMillis::new(1),
            cwd: path(),
            workspace_roots: vec![path()],
            closed: false,
        },
        foreground_operation: None,
        queued_operations: Vec::new(),
        background_operations: Vec::new(),
        operation_history: Vec::new(),
        items: Vec::new(),
        assistant_streams: Vec::new(),
        tools: Vec::new(),
        plan: SurfacePlanSnapshot {
            revision: PlanRevision::try_new(1).unwrap(),
            explanation: None,
            items: Vec::new(),
            causative_generation: None,
        },
        usage: SurfaceUsageSnapshot {
            revision: UsageRevision::try_new(1).unwrap(),
            thread_total: UsageTotals {
                input_tokens: 0,
                output_tokens: 0,
                cache_tokens: 0,
                estimated_cost_usd_micros: 0,
            },
            active_operation: None,
            goal: None,
            workflow: Vec::new(),
        },
        context: SurfaceContextSnapshot {
            revision: ContextRevision::try_new(1).unwrap(),
            used_tokens: 0,
            limit_tokens: 128_000,
            compaction: CompactionState::Idle,
            fragments: Vec::new(),
            provider_replay: ProviderReplayHealth::None,
        },
        interactions: Vec::new(),
        tasks: Vec::new(),
        workflows: Vec::new(),
        subagents: Vec::new(),
        goal: None,
        settings: SurfaceSettingsSnapshot {
            host_revision: SettingsRevision::try_new(1).unwrap(),
            thread_revision: SettingsRevision::try_new(1).unwrap(),
            effective: settings,
            pending: None,
            frozen_generation_revision: None,
        },
        mcp_catalog: SurfaceMcpCatalogSnapshot {
            revision: McpCatalogRevision::try_new(1).unwrap(),
            servers: Vec::new(),
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            diagnostics: Vec::new(),
        },
        pinned_context: SurfacePinnedContextSnapshot {
            revision: PinnedContextRevision::try_new(1).unwrap(),
            entries: Vec::new(),
        },
        session_health: SurfaceSessionHealth {
            revision: SessionHealthRevision::try_new(1).unwrap(),
            accepting_admission: true,
            issues: Vec::new(),
            closing: false,
            closed: false,
        },
    }
}

fn batch(from: u64, seed: u8) -> SurfaceCommitBatch {
    let class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        durable_revision: DurableRevision::try_new(from + 2).unwrap(),
        commit_id: SurfaceCommitId::try_from_bytes(uuid(seed)).unwrap(),
    };
    let event = SurfaceEventEnvelope {
        ordinal: 0,
        event_id: SurfaceEventId::try_from_bytes(uuid(seed.wrapping_add(1))).unwrap(),
        commit_class: class.clone(),
        scope: SurfaceScope::Thread,
        event: SurfaceEvent::Session(SessionPatch::RuntimeFault {
            class: FailureClass::Persistence,
            message: DisplayText::new(format!("fact-{seed}")),
            causative_generation: None,
        }),
    };
    let mut value = SurfaceCommitBatch {
        cursor_before: cursor(from),
        cursor_after: cursor(from + 1),
        commit_class: class,
        event_count: 1,
        batch_digest: Sha256Digest::new([0; 32]),
        events: NonEmptyVec::try_new(vec![event]).unwrap(),
    };
    value.batch_digest = canonical_batch_digest(&value);
    value
}

fn config(retained_batches: usize, subscriber_batches: usize) -> SurfaceHubConfig {
    SurfaceHubConfig {
        retained_event_limit: retained_batches as u64,
        retained_byte_limit: u64::MAX,
        subscriber_event_limit: subscriber_batches as u64,
        subscriber_byte_limit: u64::MAX,
        maximum_subscribers: 32,
    }
}

fn hub_with_config(config: SurfaceHubConfig) -> SurfaceHub {
    hub_with_snapshot(snapshot(0), config, 3)
}

fn hub_with_snapshot(
    initial_snapshot: SurfaceSnapshot,
    config: SurfaceHubConfig,
    host_seed: u8,
) -> SurfaceHub {
    SurfaceHub::new_tui(
        initial_snapshot,
        HostIncarnation::try_from_bytes(uuid(host_seed)).unwrap(),
        config,
    )
    .unwrap()
}

fn fresh_request(seed: u8) -> FreshAttachRequest {
    FreshAttachRequest {
        request_id: SurfaceRequestId::try_from_bytes(uuid(seed)).unwrap(),
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([
            SurfaceCapability::ReadSnapshot,
            SurfaceCapability::SubmitOperation,
        ]),
        interaction_capabilities: BTreeSet::from([SurfaceInteractionKind::UserInput]),
    }
}

fn cursor_request(value: SurfaceCursor, seed: u8) -> CursorAttachRequest {
    CursorAttachRequest {
        request_id: SurfaceRequestId::try_from_bytes(uuid(seed)).unwrap(),
        cursor: value,
        role: SurfaceAttachmentRole::Tui,
        requested_capabilities: BTreeSet::from([SurfaceCapability::ReadSnapshot]),
        interaction_capabilities: BTreeSet::new(),
    }
}

fn fresh_attachment(hub: &SurfaceHub, seed: u8) -> FreshSurfaceAttachment {
    match hub.attach_fresh(fresh_request(seed)) {
        AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("fresh attach failed"),
    }
}

struct FakeClock {
    seed: u8,
}

impl InjectedRuntimeClock for FakeClock {
    fn clock_id(&self) -> HostMonotonicClockId {
        HostMonotonicClockId::try_from_bytes(uuid(self.seed)).unwrap()
    }

    fn monotonic_tick(&self) -> u64 {
        self.seed as u64
    }

    fn wall_clock_ms(&self) -> i64 {
        self.seed as i64
    }
}

fn owner_lease(dir: &Path, seed: u8) -> ExclusiveOwnerLease {
    ExclusiveOwnerLease::acquire_thread(
        dir.join("thread.lock"),
        dir.join("thread.epoch"),
        cursor(0).thread_id,
        &FakeClock { seed },
    )
    .unwrap()
}

fn bound_coordinator<'owner>(
    dir: &Path,
    owner: &'owner ExclusiveOwnerLease,
    hub: &SurfaceHub,
) -> RuntimeCommitCoordinator<'owner, JsonlSurfaceCommitLedger> {
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(dir.join("surface.jsonl"), cursor(0)),
        SurfaceReducerState::new(snapshot(0)),
        owner,
    )
    .unwrap();
    coordinator.bind_surface_hub(hub.clone()).unwrap();
    coordinator
}

#[test]
fn attach_is_unavailable_until_authoritative_coordinator_bind() {
    let hub = hub_with_config(config(8, 8));

    assert!(matches!(
        hub.attach_fresh(fresh_request(18)),
        AttachResult::Unavailable {
            reason: SurfaceUnavailableReason::RuntimeUnavailable
        }
    ));
    assert!(matches!(
        hub.attach_after(cursor_request(cursor(0), 19)),
        AttachResult::Unavailable {
            reason: SurfaceUnavailableReason::RuntimeUnavailable
        }
    ));
    assert_eq!(hub.subscriber_count(), 0);
}

#[test]
fn fresh_attach_is_atomic_and_subscription_starts_strictly_after_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 8));
    let owner = owner_lease(dir.path(), 9);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let first = batch(0, 10);
    coordinator.commit_actor_batch(&first).unwrap();

    let attachment = fresh_attachment(&hub, 20);
    let cloned_subscription = attachment.subscription.clone();
    let mut subscription = hub.claim_subscription(&attachment.subscription).unwrap();
    assert!(hub.claim_subscription(&cloned_subscription).is_none());
    assert_eq!(attachment.baseline.cursor, first.cursor_after);
    assert_eq!(attachment.baseline.snapshot.cursor, first.cursor_after);
    assert!(subscription.try_recv().is_none());

    let second = batch(1, 11);
    coordinator.commit_actor_batch(&second).unwrap();
    assert!(matches!(
        subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Batch { batch }) if batch == second
    ));
}

#[test]
fn cursor_attach_returns_half_open_replay_without_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 8));
    let owner = owner_lease(dir.path(), 29);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let first = batch(0, 30);
    let second = batch(1, 31);
    coordinator.commit_actor_batch(&first).unwrap();
    coordinator.commit_actor_batch(&second).unwrap();

    let attachment = match hub.attach_after(cursor_request(cursor(0), 32)) {
        AttachResult::CursorAttached { attachment } => attachment,
        _ => panic!("cursor attach failed"),
    };
    let mut subscription = hub.claim_subscription(&attachment.subscription).unwrap();
    assert_eq!(attachment.from, cursor(0));
    assert_eq!(attachment.head, cursor(2));
    assert!(attachment.replay == vec![first, second]);
    assert!(subscription.try_recv().is_none());
}

#[test]
fn retained_suffix_is_half_open_and_expires_only_evicted_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(2, 8));
    let owner = owner_lease(dir.path(), 39);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    for index in 0..3 {
        coordinator
            .commit_actor_batch(&batch(index, 40 + index as u8))
            .unwrap();
    }
    assert!(matches!(
        hub.attach_after(cursor_request(cursor(0), 50)),
        AttachResult::SnapshotRequired { required }
            if required.reason == SnapshotRequiredReason::ExpiredSuffix
    ));
    let replay = match hub.attach_after(cursor_request(cursor(1), 51)) {
        AttachResult::CursorAttached { attachment } => attachment.replay,
        _ => panic!("retained boundary rejected"),
    };
    assert_eq!(replay.len(), 2);
    assert_eq!(replay[0].cursor_before, cursor(1));
    assert_eq!(replay[1].cursor_after, cursor(3));
}

#[test]
fn attach_concurrent_with_commit_has_no_duplicate_or_missing_batch() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 8));
    let owner = ExclusiveOwnerLease::acquire_thread(
        dir.path().join("thread.lock"),
        dir.path().join("thread.epoch"),
        cursor(0).thread_id,
        &FakeClock { seed: 59 },
    )
    .unwrap();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(dir.path().join("surface.jsonl"), cursor(0)),
        SurfaceReducerState::new(snapshot(0)),
        &owner,
    )
    .unwrap();
    coordinator.bind_surface_hub(hub.clone()).unwrap();
    let committed = batch(0, 60);

    thread::scope(|scope| {
        let expected = committed.clone();
        let join = scope.spawn(move || {
            coordinator.commit_actor_batch(&expected).unwrap();
        });
        let attachment = fresh_attachment(&hub, 61);
        let mut subscription = hub.claim_subscription(&attachment.subscription).unwrap();
        join.join().unwrap();
        let baseline_has_batch = attachment.baseline.cursor == committed.cursor_after;
        let live_has_batch = matches!(
            subscription.try_recv(),
            Some(SurfaceSubscriptionItem::Batch { batch }) if batch == committed
        );
        assert_ne!(baseline_has_batch, live_has_batch);
    });
}

#[test]
fn recovered_coordinator_repairs_append_before_publish_from_exact_batches() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("thread.lock");
    let epoch_path = dir.path().join("thread.epoch");
    let ledger_path = dir.path().join("surface.jsonl");
    let hub = hub_with_config(config(8, 8));
    let committed = batch(0, 76);

    {
        let owner = ExclusiveOwnerLease::acquire_thread(
            &lock_path,
            &epoch_path,
            cursor(0).thread_id,
            &FakeClock { seed: 77 },
        )
        .unwrap();
        let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
            JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
            SurfaceReducerState::new(snapshot(0)),
            &owner,
        )
        .unwrap();
        coordinator.commit_actor_batch(&committed).unwrap();
    }

    let owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &FakeClock { seed: 78 },
    )
    .unwrap();
    let mut recovered = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot(0)),
        &owner,
    )
    .unwrap();
    recovered.bind_surface_hub(hub.clone()).unwrap();

    let replay = match hub.attach_after(cursor_request(cursor(0), 79)) {
        AttachResult::CursorAttached { attachment } => attachment.replay,
        _ => panic!("recovered suffix was not rebuilt"),
    };
    assert!(replay == vec![committed]);
}

#[test]
fn recovered_coordinator_buffers_post_recovery_commits_until_hub_is_bound() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("thread.lock");
    let epoch_path = dir.path().join("thread.epoch");
    let ledger_path = dir.path().join("surface.jsonl");
    let owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &FakeClock { seed: 160 },
    )
    .unwrap();
    let first = batch(0, 161);
    {
        let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
            JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
            SurfaceReducerState::new(snapshot(0)),
            &owner,
        )
        .unwrap();
        coordinator.commit_actor_batch(&first).unwrap();
    }
    let mut recovered = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot(0)),
        &owner,
    )
    .unwrap();
    let second = batch(1, 162);

    recovered.commit_actor_batch(&second).unwrap();
    assert_eq!(recovered.state().snapshot().cursor, cursor(2));

    let hub = hub_with_config(config(8, 8));
    recovered.bind_surface_hub(hub.clone()).unwrap();
    let replay = match hub.attach_after(cursor_request(cursor(0), 163)) {
        AttachResult::CursorAttached { attachment } => attachment.replay,
        _ => panic!("buffered recovered suffix was not published at bind"),
    };
    assert!(replay == vec![first, second]);
}

#[test]
fn binding_rejects_wrong_thread_and_rebinding_without_orphaning_subscribers() {
    let dir = tempfile::tempdir().unwrap();
    let owner = ExclusiveOwnerLease::acquire_thread(
        dir.path().join("thread.lock"),
        dir.path().join("thread.epoch"),
        cursor(0).thread_id,
        &FakeClock { seed: 170 },
    )
    .unwrap();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(dir.path().join("surface.jsonl"), cursor(0)),
        SurfaceReducerState::new(snapshot(0)),
        &owner,
    )
    .unwrap();
    let mut wrong_snapshot = snapshot(0);
    let wrong_thread = SurfaceThreadId::try_from_bytes([9; 16]).unwrap();
    wrong_snapshot.cursor.thread_id = wrong_thread.clone();
    wrong_snapshot.thread.thread_id = wrong_thread;
    let wrong_hub = hub_with_snapshot(wrong_snapshot, config(8, 8), 171);
    assert_eq!(
        coordinator.bind_surface_hub(wrong_hub.clone()),
        Err(SurfaceHubBindError::WrongThread)
    );
    assert!(matches!(
        wrong_hub.attach_fresh(fresh_request(172)),
        AttachResult::Unavailable {
            reason: SurfaceUnavailableReason::RuntimeUnavailable
        }
    ));
    assert_eq!(wrong_hub.subscriber_count(), 0);

    let bound_hub = hub_with_config(config(8, 8));
    coordinator.bind_surface_hub(bound_hub.clone()).unwrap();
    let bound_attachment = fresh_attachment(&bound_hub, 173);
    let mut bound_subscription = bound_hub
        .claim_subscription(&bound_attachment.subscription)
        .unwrap();
    assert_eq!(
        coordinator.bind_surface_hub(wrong_hub.clone()),
        Err(SurfaceHubBindError::AlreadyBound)
    );
    let committed = batch(0, 174);
    coordinator.commit_actor_batch(&committed).unwrap();
    assert!(matches!(
        bound_subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Batch { batch }) if batch == committed
    ));
    assert!(matches!(
        wrong_hub.attach_fresh(fresh_request(175)),
        AttachResult::Unavailable {
            reason: SurfaceUnavailableReason::RuntimeUnavailable
        }
    ));
}

#[test]
fn binding_replaces_stale_same_cursor_snapshot_with_authoritative_state() {
    let dir = tempfile::tempdir().unwrap();
    let owner = ExclusiveOwnerLease::acquire_thread(
        dir.path().join("thread.lock"),
        dir.path().join("thread.epoch"),
        cursor(0).thread_id,
        &FakeClock { seed: 180 },
    )
    .unwrap();
    let mut authoritative = snapshot(0);
    authoritative.thread.title = DisplayText::new("authoritative");
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(dir.path().join("surface.jsonl"), cursor(0)),
        SurfaceReducerState::new(authoritative),
        &owner,
    )
    .unwrap();
    let mut stale = snapshot(0);
    stale.thread.title = DisplayText::new("stale");
    let hub = hub_with_snapshot(stale, config(8, 8), 181);

    coordinator.bind_surface_hub(hub.clone()).unwrap();

    let attachment = fresh_attachment(&hub, 182);
    assert_eq!(
        attachment.baseline.snapshot.thread.title,
        DisplayText::new("authoritative")
    );
}

#[test]
fn recovery_initialization_rebuilds_suffix_from_exact_batches_at_final_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let owner = owner_lease(dir.path(), 84);
    let recovered = batch(0, 85);
    {
        let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
            JsonlSurfaceCommitLedger::new(dir.path().join("surface.jsonl"), cursor(0)),
            SurfaceReducerState::new(snapshot(0)),
            &owner,
        )
        .unwrap();
        coordinator.commit_actor_batch(&recovered).unwrap();
    }
    let mut coordinator = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(dir.path().join("surface.jsonl"), cursor(0)),
        SurfaceReducerState::new(snapshot(0)),
        &owner,
    )
    .unwrap();
    let hub = hub_with_snapshot(coordinator.state().snapshot().clone(), config(8, 8), 86);

    coordinator.bind_surface_hub(hub.clone()).unwrap();

    let replay = match hub.attach_after(cursor_request(cursor(0), 87)) {
        AttachResult::CursorAttached { attachment } => attachment.replay,
        _ => panic!("recovered suffix was not initialized"),
    };
    assert!(replay == vec![recovered]);
}

#[test]
fn discontinuous_commit_signals_replay_hole() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 8));
    let owner = owner_lease(dir.path(), 81);
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(dir.path().join("surface.jsonl"), cursor(0)),
        SurfaceReducerState::new(snapshot(3)),
        &owner,
    )
    .unwrap();
    coordinator.bind_surface_hub(hub.clone()).unwrap();
    assert!(matches!(
        hub.attach_after(cursor_request(cursor(0), 82)),
        AttachResult::SnapshotRequired { required }
            if required.reason == SnapshotRequiredReason::ReplayHole
    ));
}

#[test]
fn stale_incarnation_requires_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 8));
    let owner = owner_lease(dir.path(), 89);
    let _coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let mut stale = cursor(0);
    stale.incarnation = SurfaceIncarnation::try_from_bytes(uuid(90)).unwrap();
    assert!(matches!(
        hub.attach_after(cursor_request(stale, 91)),
        AttachResult::SnapshotRequired { required }
            if required.reason == SnapshotRequiredReason::StaleIncarnation
    ));
}

#[test]
fn future_and_wrong_thread_cursors_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 8));
    let owner = owner_lease(dir.path(), 99);
    let _coordinator = bound_coordinator(dir.path(), &owner, &hub);
    assert!(matches!(
        hub.attach_after(cursor_request(cursor(1), 100)),
        AttachResult::InvalidCursor { error }
            if error.reason == InvalidCursorReason::FutureSequence
    ));
    let mut wrong = cursor(0);
    wrong.thread_id = SurfaceThreadId::try_from_bytes([9; 16]).unwrap();
    assert!(matches!(
        hub.attach_after(cursor_request(wrong, 101)),
        AttachResult::InvalidCursor { error }
            if error.reason == InvalidCursorReason::WrongThread
    ));
}

#[test]
fn non_boundary_and_impossible_revision_cursors_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 8));
    let owner = owner_lease(dir.path(), 109);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let mut two_event = batch(0, 110);
    let second_event = SurfaceEventEnvelope {
        ordinal: 1,
        event_id: SurfaceEventId::try_from_bytes(uuid(112)).unwrap(),
        commit_class: two_event.commit_class.clone(),
        scope: SurfaceScope::Thread,
        event: SurfaceEvent::Session(SessionPatch::RuntimeFault {
            class: FailureClass::Persistence,
            message: DisplayText::new("second"),
            causative_generation: None,
        }),
    };
    two_event.events =
        NonEmptyVec::try_new(vec![two_event.events.as_slice()[0].clone(), second_event]).unwrap();
    two_event.event_count = 2;
    two_event.cursor_after = SurfaceCursor {
        next_seq: SequenceNumber::new(2),
        ..cursor(1)
    };
    two_event.batch_digest = canonical_batch_digest(&two_event);
    coordinator.commit_actor_batch(&two_event).unwrap();

    assert!(matches!(
        hub.attach_after(cursor_request(cursor(1), 113)),
        AttachResult::InvalidCursor { error }
            if error.reason == InvalidCursorReason::NotBatchBoundary
    ));
    let mut impossible = cursor(0);
    impossible.source_revision = CursorSourceRevision::Recorded {
        durable_revision: DurableRevision::try_new(99).unwrap(),
    };
    assert!(matches!(
        hub.attach_after(cursor_request(impossible, 114)),
        AttachResult::InvalidCursor { error }
            if error.reason == InvalidCursorReason::ImpossibleSourceRevision
    ));
}

#[test]
fn bounded_subscriber_overflow_yields_one_slow_subscriber_gap() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 1));
    let owner = owner_lease(dir.path(), 119);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let attachment = fresh_attachment(&hub, 120);
    let mut subscription = hub.claim_subscription(&attachment.subscription).unwrap();
    let admitted = batch(0, 121);
    coordinator.commit_actor_batch(&admitted).unwrap();
    coordinator.commit_actor_batch(&batch(1, 122)).unwrap();
    assert!(matches!(
        subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Batch { batch }) if batch == admitted
    ));
    assert!(matches!(
        subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Gap { required })
            if required.reason == SnapshotRequiredReason::SlowSubscriber
    ));
    assert!(subscription.try_recv().is_none());
    assert_eq!(hub.subscriber_count(), 0);
}

#[test]
fn drained_slow_subscribers_do_not_exhaust_retired_lane_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let mut limited = config(8, 1);
    limited.maximum_subscribers = 1;
    let hub = hub_with_config(limited);
    let owner = owner_lease(dir.path(), 220);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);

    for cycle in 0..5_u64 {
        let attachment = fresh_attachment(&hub, 221 + cycle as u8);
        let mut subscription = hub.claim_subscription(&attachment.subscription).unwrap();
        let admitted = batch(cycle * 2, 230 + (cycle * 2) as u8);
        coordinator.commit_actor_batch(&admitted).unwrap();
        coordinator
            .commit_actor_batch(&batch(cycle * 2 + 1, 231 + (cycle * 2) as u8))
            .unwrap();

        assert!(matches!(
            subscription.try_recv(),
            Some(SurfaceSubscriptionItem::Batch { batch }) if batch == admitted
        ));
        assert!(matches!(
            subscription.try_recv(),
            Some(SurfaceSubscriptionItem::Gap { required })
                if required.reason == SnapshotRequiredReason::SlowSubscriber
        ));
        assert!(subscription.try_recv().is_none());
    }

    let _still_available = fresh_attachment(&hub, 250);
}

#[test]
fn dropped_subscription_receivers_reclaim_lane_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let mut limited = config(8, 1);
    limited.maximum_subscribers = 1;
    let hub = hub_with_config(limited);
    let owner = owner_lease(dir.path(), 200);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);

    for cycle in 0..5_u64 {
        let attachment = fresh_attachment(&hub, 201 + cycle as u8);
        let subscription = hub.claim_subscription(&attachment.subscription).unwrap();
        coordinator
            .commit_actor_batch(&batch(cycle * 2, 210 + (cycle * 2) as u8))
            .unwrap();
        coordinator
            .commit_actor_batch(&batch(cycle * 2 + 1, 211 + (cycle * 2) as u8))
            .unwrap();
        drop(subscription);
    }

    let _still_available = fresh_attachment(&hub, 225);
}

#[test]
fn unclaimed_fresh_attachments_reclaim_overflowed_lanes_on_last_owner_drop() {
    let dir = tempfile::tempdir().unwrap();
    let mut limited = config(16, 1);
    limited.maximum_subscribers = 1;
    let hub = hub_with_config(limited);
    let owner = owner_lease(dir.path(), 10);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);

    for cycle in 0..5_u64 {
        let attachment = fresh_attachment(&hub, 11 + cycle as u8);
        coordinator
            .commit_actor_batch(&batch(cycle * 2, 20 + (cycle * 2) as u8))
            .unwrap();
        coordinator
            .commit_actor_batch(&batch(cycle * 2 + 1, 21 + (cycle * 2) as u8))
            .unwrap();
        drop(attachment);
    }

    let _still_available = fresh_attachment(&hub, 40);
}

#[test]
fn unclaimed_cursor_attachment_reclaims_active_lane_on_last_owner_drop() {
    let dir = tempfile::tempdir().unwrap();
    let mut limited = config(8, 8);
    limited.maximum_subscribers = 1;
    let hub = hub_with_config(limited);
    let owner = owner_lease(dir.path(), 41);
    let _coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let attachment = match hub.attach_after(cursor_request(cursor(0), 42)) {
        AttachResult::CursorAttached { attachment } => attachment,
        _ => panic!("cursor attach failed"),
    };

    drop(attachment);

    let _still_available = fresh_attachment(&hub, 43);
}

#[test]
fn subscription_lane_is_reclaimed_only_after_the_last_handle_clone_drops() {
    let dir = tempfile::tempdir().unwrap();
    let mut limited = config(8, 8);
    limited.maximum_subscribers = 1;
    let hub = hub_with_config(limited);
    let owner = owner_lease(dir.path(), 44);
    let _coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let attachment = fresh_attachment(&hub, 45);
    let surviving_handle = attachment.subscription.clone();

    drop(attachment);
    assert!(matches!(
        hub.attach_fresh(fresh_request(46)),
        AttachResult::Unavailable {
            reason: SurfaceUnavailableReason::CapacityExceeded
        }
    ));

    drop(surviving_handle);
    let _still_available = fresh_attachment(&hub, 47);
}

#[test]
fn claim_atomically_transfers_lane_ownership_to_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let mut limited = config(8, 8);
    limited.maximum_subscribers = 1;
    let hub = hub_with_config(limited);
    let owner = owner_lease(dir.path(), 48);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let attachment = fresh_attachment(&hub, 49);
    let mut receiver = hub.claim_subscription(&attachment.subscription).unwrap();

    drop(attachment);
    assert!(matches!(
        hub.attach_fresh(fresh_request(50)),
        AttachResult::Unavailable {
            reason: SurfaceUnavailableReason::CapacityExceeded
        }
    ));
    let committed = batch(0, 51);
    coordinator.commit_actor_batch(&committed).unwrap();
    assert!(matches!(
        receiver.try_recv(),
        Some(SurfaceSubscriptionItem::Batch { batch }) if batch == committed
    ));

    drop(receiver);
    let _still_available = fresh_attachment(&hub, 52);
}

#[test]
fn slow_subscriber_gap_reports_the_post_trim_retained_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(1, 1));
    let owner = owner_lease(dir.path(), 124);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let attachment = fresh_attachment(&hub, 125);
    let mut subscription = hub.claim_subscription(&attachment.subscription).unwrap();
    coordinator.commit_actor_batch(&batch(0, 126)).unwrap();
    coordinator.commit_actor_batch(&batch(1, 127)).unwrap();

    assert!(matches!(
        subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Batch { .. })
    ));
    assert!(matches!(
        subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Gap { required })
            if required.reason == SnapshotRequiredReason::SlowSubscriber
                && required.retained_from == Some(cursor(1))
    ));
}

#[test]
fn subscriber_registry_capacity_fails_without_partial_registration() {
    let dir = tempfile::tempdir().unwrap();
    let mut limited = config(8, 1);
    limited.maximum_subscribers = 1;
    let hub = hub_with_config(limited);
    let owner = owner_lease(dir.path(), 129);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let first = fresh_attachment(&hub, 130);
    let mut first_subscription = hub.claim_subscription(&first.subscription).unwrap();
    assert!(matches!(
        hub.attach_fresh(fresh_request(131)),
        AttachResult::Unavailable {
            reason: SurfaceUnavailableReason::CapacityExceeded
        }
    ));
    assert_eq!(hub.subscriber_count(), 1);
    coordinator.commit_actor_batch(&batch(0, 132)).unwrap();
    coordinator.commit_actor_batch(&batch(1, 133)).unwrap();
    assert_eq!(hub.subscriber_count(), 0);
    let _replacement = fresh_attachment(&hub, 134);
    assert!(matches!(
        first_subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Batch { .. })
    ));
    assert!(matches!(
        first_subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Gap { .. })
    ));
}

#[test]
fn detach_revokes_subscription_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 8));
    let owner = owner_lease(dir.path(), 139);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let attachment = fresh_attachment(&hub, 140);
    let mut subscription = hub.claim_subscription(&attachment.subscription).unwrap();
    let request = DetachRequest {
        request_id: SurfaceRequestId::try_from_bytes(uuid(141)).unwrap(),
    };
    assert!(matches!(
        hub.detach(&attachment.client, request.clone()),
        DetachResult::Detached { .. }
    ));
    assert!(matches!(
        hub.detach(&attachment.client, request),
        DetachResult::AlreadyDetached { .. }
    ));
    coordinator.commit_actor_batch(&batch(0, 142)).unwrap();
    assert!(subscription.try_recv().is_none());
}

#[test]
fn detach_idempotency_survives_unbounded_attachment_churn() {
    let dir = tempfile::tempdir().unwrap();
    let mut limited = config(8, 8);
    limited.maximum_subscribers = 1;
    let hub = hub_with_config(limited);
    let owner = owner_lease(dir.path(), 180);
    let _coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let first = fresh_attachment(&hub, 181);
    let first_request = DetachRequest {
        request_id: SurfaceRequestId::try_from_bytes(uuid(182)).unwrap(),
    };
    let first_receipt = match hub.detach(&first.client, first_request.clone()) {
        DetachResult::Detached { receipt } => receipt,
        _ => panic!("first detach failed"),
    };

    for seed in 183..190 {
        let attachment = fresh_attachment(&hub, seed);
        let request = DetachRequest {
            request_id: SurfaceRequestId::try_from_bytes(uuid(seed + 20)).unwrap(),
        };
        assert!(matches!(
            hub.detach(&attachment.client, request),
            DetachResult::Detached { .. }
        ));
    }

    assert!(matches!(
        hub.detach(&first.client, first_request),
        DetachResult::AlreadyDetached { receipt } if receipt == first_receipt
    ));
}

#[test]
fn slow_client_cannot_block_writer_or_other_client() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub_with_config(config(8, 1));
    let owner = owner_lease(dir.path(), 149);
    let mut coordinator = bound_coordinator(dir.path(), &owner, &hub);
    let slow = fresh_attachment(&hub, 150);
    let fast = fresh_attachment(&hub, 151);
    let mut slow_subscription = hub.claim_subscription(&slow.subscription).unwrap();
    let mut fast_subscription = hub.claim_subscription(&fast.subscription).unwrap();
    for index in 0..4 {
        let value = batch(index, 152 + index as u8);
        coordinator.commit_actor_batch(&value).unwrap();
        assert!(matches!(
            fast_subscription.try_recv(),
            Some(SurfaceSubscriptionItem::Batch { batch }) if batch == value
        ));
    }
    assert!(matches!(
        slow_subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Batch { .. })
    ));
    assert!(matches!(
        slow_subscription.try_recv(),
        Some(SurfaceSubscriptionItem::Gap { required })
            if required.reason == SnapshotRequiredReason::SlowSubscriber
    ));
    assert_eq!(fresh_attachment(&hub, 160).baseline.cursor, cursor(4));
}
