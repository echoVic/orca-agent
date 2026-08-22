use orca_runtime::surface::*;
use sha2::Digest;

const MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json"
));

fn uuid(seed: u8) -> [u8; 16] {
    let mut bytes = [seed; 16];
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    bytes
}

fn digest(seed: u8) -> Sha256Digest {
    Sha256Digest::new([seed; 32])
}

fn path() -> CanonicalPath {
    CanonicalPath::try_new(std::env::temp_dir().join("orca-surface-commit")).unwrap()
}

fn cursor(next_seq: u64) -> SurfaceCursor {
    SurfaceCursor {
        thread_id: SurfaceThreadId::try_from_bytes([1; 16]).unwrap(),
        incarnation: SurfaceIncarnation::try_from_bytes(uuid(2)).unwrap(),
        next_seq: SequenceNumber::new(next_seq),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(1).unwrap(),
        },
    }
}

fn same_process_reset() -> MaterializationCause {
    MaterializationCause::SameProcessProjectionReset {
        retained_incarnation: cursor(0).incarnation,
    }
}

fn actor_permit(thread_id: SurfaceThreadId, epoch: u64) -> SurfacePublisherPermit {
    // The frozen permit id is intentionally opaque to external callers; only its authority
    // fields are relevant to these coordinator integration tests.
    let permit_id =
        unsafe { std::mem::MaybeUninit::<SurfacePublisherPermitId>::zeroed().assume_init() };
    SurfacePublisherPermit::ActorControl {
        permit_id,
        thread_id,
        owner_epoch: ThreadOwnerEpoch::new(epoch),
    }
}

fn background_owner_token() -> SurfaceBackgroundOwnerToken {
    // The token is opaque outside the runtime; integration tests only need a stable identity.
    unsafe { std::mem::MaybeUninit::<SurfaceBackgroundOwnerToken>::zeroed().assume_init() }
}

fn snapshot() -> SurfaceSnapshot {
    let settings = SurfaceRuntimeSettings {
        model: NonEmptyText::try_new("deepseek-v4").unwrap(),
        reasoning_effort: SurfaceReasoningEffort::High,
        approval_mode: SurfaceApprovalMode::AutoEdit,
        cwd: path(),
        workspace_roots: vec![path()],
        active_permission_profile: None,
        permission_rules: SurfacePermissionRuleSet {
            ordered_rules: Vec::new(),
            digest: digest(1),
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
        cursor: cursor(0),
        thread: SurfaceThreadSnapshot {
            thread_id: cursor(0).thread_id,
            owner_epoch: ThreadOwnerEpoch::new(1),
            persistence: ThreadPersistence::RecordedCatalogued,
            title: DisplayText::new("commit test"),
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
            window_id: orca_runtime::surface::ContextWindowId::new(),
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

fn batch(seed: u8) -> SurfaceCommitBatch {
    let class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        durable_revision: DurableRevision::try_new(2).unwrap(),
        commit_id: SurfaceCommitId::try_from_bytes(uuid(seed)).unwrap(),
    };
    let event = SurfaceEventEnvelope {
        ordinal: 0,
        event_id: SurfaceEventId::try_from_bytes(uuid(seed.wrapping_add(1))).unwrap(),
        commit_class: class.clone(),
        scope: SurfaceScope::Thread,
        event: SurfaceEvent::Session(SessionPatch::RuntimeFault {
            class: FailureClass::Persistence,
            message: DisplayText::new("durable fact"),
            causative_generation: None,
        }),
    };
    let mut batch = SurfaceCommitBatch {
        cursor_before: cursor(0),
        cursor_after: SurfaceCursor {
            next_seq: SequenceNumber::new(1),
            source_revision: CursorSourceRevision::Recorded {
                durable_revision: DurableRevision::try_new(2).unwrap(),
            },
            ..cursor(0)
        },
        commit_class: class,
        event_count: 1,
        batch_digest: digest(0),
        events: NonEmptyVec::try_new(vec![event]).unwrap(),
    };
    batch.batch_digest = canonical_batch_digest(&batch);
    batch
}

fn ephemeral_cursor(next_seq: u64, live_revision: u64) -> SurfaceCursor {
    SurfaceCursor {
        thread_id: SurfaceThreadId::try_from_bytes(uuid(81)).unwrap(),
        incarnation: SurfaceIncarnation::try_from_bytes(uuid(82)).unwrap(),
        next_seq: SequenceNumber::new(next_seq),
        source_revision: CursorSourceRevision::Ephemeral {
            live_revision: LiveRevision::try_new(live_revision).unwrap(),
        },
    }
}

fn ephemeral_batch(seed: u8) -> SurfaceCommitBatch {
    let cursor_before = ephemeral_cursor(0, 1);
    let live_revision = LiveRevision::try_new(2).unwrap();
    let class = CommitClass::Ephemeral {
        incarnation: cursor_before.incarnation.clone(),
        live_revision,
        commit_id: SurfaceCommitId::try_from_bytes(uuid(seed)).unwrap(),
    };
    let event = SurfaceEventEnvelope {
        ordinal: 0,
        event_id: SurfaceEventId::try_from_bytes(uuid(seed.wrapping_add(1))).unwrap(),
        commit_class: class.clone(),
        scope: SurfaceScope::Thread,
        event: SurfaceEvent::Session(SessionPatch::RuntimeFault {
            class: FailureClass::RuntimeInvariant,
            message: DisplayText::new("ephemeral fact"),
            causative_generation: None,
        }),
    };
    let mut batch = SurfaceCommitBatch {
        cursor_before: cursor_before.clone(),
        cursor_after: SurfaceCursor {
            next_seq: SequenceNumber::new(1),
            source_revision: CursorSourceRevision::Ephemeral { live_revision },
            ..cursor_before
        },
        commit_class: class,
        event_count: 1,
        batch_digest: digest(0),
        events: NonEmptyVec::try_new(vec![event]).unwrap(),
    };
    batch.batch_digest = canonical_batch_digest(&batch);
    batch
}

#[test]
fn in_memory_ledger_commits_ephemeral_batches_without_fabricating_durability() {
    let initial = ephemeral_cursor(0, 1);
    let mut ledger = InMemorySurfaceCommitLedger::new(initial);
    let batch = ephemeral_batch(83);

    let receipt = ledger
        .append_complete_batch(&batch)
        .expect("append ephemeral batch");
    assert_eq!(
        receipt,
        SurfaceBatchReceipt::Ephemeral(EphemeralBatchReceipt {
            commit_id: SurfaceCommitId::try_from_bytes(uuid(83)).unwrap(),
            live_revision: LiveRevision::try_new(2).unwrap(),
            event_count: 1,
            batch_digest: batch.batch_digest.clone(),
            cursor_after: batch.cursor_after.clone(),
        })
    );
    ledger.checkpoint(&receipt).expect("checkpoint in memory");
    assert_eq!(
        ledger.probe_commit(
            match &batch.commit_class {
                CommitClass::Ephemeral { commit_id, .. } => commit_id,
                CommitClass::Recorded { .. } => unreachable!(),
            },
            &batch.batch_digest,
        ),
        CommitProbe::Present(receipt.clone())
    );
    assert_eq!(ledger.append_complete_batch(&batch), Ok(receipt));
}

#[test]
fn in_memory_ledger_rejects_recorded_wrong_cursor_and_conflicting_identity() {
    let initial = ephemeral_cursor(0, 1);
    let mut recorded_ledger = InMemorySurfaceCommitLedger::new(initial.clone());
    assert_eq!(
        recorded_ledger.append_complete_batch(&batch(84)),
        Err(SurfaceLedgerError::CommitIdentityConflict)
    );

    let mut wrong_cursor = ephemeral_batch(85);
    wrong_cursor.cursor_before.next_seq = SequenceNumber::new(7);
    wrong_cursor.batch_digest = canonical_batch_digest(&wrong_cursor);
    let mut cursor_ledger = InMemorySurfaceCommitLedger::new(initial.clone());
    assert_eq!(
        cursor_ledger.append_complete_batch(&wrong_cursor),
        Err(SurfaceLedgerError::CommitIdentityConflict)
    );

    let mut ledger = InMemorySurfaceCommitLedger::new(initial);
    let original = ephemeral_batch(86);
    ledger
        .append_complete_batch(&original)
        .expect("append original batch");
    let mut conflict = original.clone();
    let mut event = conflict.events.as_slice()[0].clone();
    event.event = SurfaceEvent::Session(SessionPatch::RuntimeFault {
        class: FailureClass::RuntimeInvariant,
        message: DisplayText::new("different fact"),
        causative_generation: None,
    });
    conflict.events = NonEmptyVec::try_new(vec![event]).unwrap();
    conflict.batch_digest = canonical_batch_digest(&conflict);
    assert_eq!(
        ledger.append_complete_batch(&conflict),
        Err(SurfaceLedgerError::CommitIdentityConflict)
    );
}

fn reservation(operation_id: &SurfaceOperationId, seed: u8) -> ReservationLease {
    serde_json::from_value(serde_json::json!({
        "lease_id": SurfaceAdmissionLeaseId::try_from_bytes(uuid(seed)).unwrap(),
        "operation_id": operation_id,
        "reservation_sequence": 1,
        "issuing_host_incarnation": HostIncarnation::try_from_bytes(uuid(seed.wrapping_add(1))).unwrap(),
        "issued_at": {
            "clock_id": HostMonotonicClockId::try_from_bytes(uuid(seed.wrapping_add(2))).unwrap(),
            "tick": 1
        },
        "duration": SURFACE_RESERVATION_LEASE_MS
    }))
    .unwrap()
}

fn requested_operation(seed: u8) -> OperationRecord {
    let operation_id = SurfaceOperationId::try_from_bytes(uuid(seed)).unwrap();
    OperationRecord {
        operation_id: operation_id.clone(),
        request_id: SurfaceRequestId::try_from_bytes(uuid(seed.wrapping_add(1))).unwrap(),
        intent: OperationIntent {
            origin: OperationOrigin::TuiUser,
            kind: OperationKind::ManualCompaction {
                reason: ManualCompactionReason::Manual,
            },
            initial_replayability: Replayability::NonReplayable {
                reason: NonReplayableReason::HistoryDisabled,
                live_capsule: LiveOperationCapsule::Unavailable,
            },
            busy_disposition: BusyDisposition::Queue,
            interrupt_settlement: InterruptSettlement::SuspendUntilExplicitControl,
            legacy_visibility: LegacyVisibility::PublishAfterAdmitted,
            settings_revision: SettingsRevision::try_new(1).unwrap(),
            policy_epoch: PolicyEpoch::try_new(1).unwrap(),
            required_capabilities: Default::default(),
            capability_fingerprint: digest(seed),
            settings_receipt: OperationSettingsPreparationReceipt::Current {
                settings_revision: SettingsRevision::try_new(1).unwrap(),
                policy_epoch: PolicyEpoch::try_new(1).unwrap(),
            },
        },
        phase: OperationPhase::Requested,
        reservation: reservation(&operation_id, seed.wrapping_add(2)),
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

fn operation_batch(seed: u8, operation: OperationRecord) -> SurfaceCommitBatch {
    let class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        durable_revision: DurableRevision::try_new(2).unwrap(),
        commit_id: SurfaceCommitId::try_from_bytes(uuid(seed)).unwrap(),
    };
    let event = SurfaceEventEnvelope {
        ordinal: 0,
        event_id: SurfaceEventId::try_from_bytes(uuid(seed.wrapping_add(1))).unwrap(),
        commit_class: class.clone(),
        scope: SurfaceScope::Operation {
            operation_id: operation.operation_id.clone(),
        },
        event: SurfaceEvent::Operation(OperationPatch::Requested { operation }),
    };
    let mut batch = SurfaceCommitBatch {
        cursor_before: cursor(0),
        cursor_after: SurfaceCursor {
            next_seq: SequenceNumber::new(1),
            source_revision: CursorSourceRevision::Recorded {
                durable_revision: DurableRevision::try_new(2).unwrap(),
            },
            ..cursor(0)
        },
        commit_class: class,
        event_count: 1,
        batch_digest: digest(0),
        events: NonEmptyVec::try_new(vec![event]).unwrap(),
    };
    batch.batch_digest = canonical_batch_digest(&batch);
    batch
}

fn next_operation_batch(
    seed: u8,
    cursor_before: SurfaceCursor,
    operation_id: SurfaceOperationId,
    patches: Vec<OperationPatch>,
) -> SurfaceCommitBatch {
    let durable_revision = match cursor_before.source_revision {
        CursorSourceRevision::Recorded { durable_revision } => {
            DurableRevision::try_new(durable_revision.get() + 1).unwrap()
        }
        CursorSourceRevision::Ephemeral { .. } => panic!("test requires recorded cursor"),
    };
    let class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        durable_revision,
        commit_id: SurfaceCommitId::try_from_bytes(uuid(seed)).unwrap(),
    };
    let event_count = patches.len() as u32;
    let events = patches
        .into_iter()
        .enumerate()
        .map(|(ordinal, patch)| SurfaceEventEnvelope {
            ordinal: ordinal as u32,
            event_id: SurfaceEventId::try_from_bytes(uuid(
                seed.wrapping_add(ordinal as u8).wrapping_add(1),
            ))
            .unwrap(),
            commit_class: class.clone(),
            scope: match &patch {
                OperationPatch::GenerationStarted { fence, .. }
                | OperationPatch::GenerationStopped { fence, .. }
                | OperationPatch::GenerationTransferred { fence, .. } => SurfaceScope::Generation {
                    fence: fence.clone(),
                },
                _ => SurfaceScope::Operation {
                    operation_id: operation_id.clone(),
                },
            },
            event: SurfaceEvent::Operation(patch),
        })
        .collect::<Vec<_>>();
    let mut batch = SurfaceCommitBatch {
        cursor_after: SurfaceCursor {
            next_seq: SequenceNumber::new(cursor_before.next_seq.get() + event_count as u64),
            source_revision: CursorSourceRevision::Recorded { durable_revision },
            ..cursor_before.clone()
        },
        cursor_before,
        commit_class: class,
        event_count,
        batch_digest: digest(0),
        events: NonEmptyVec::try_new(events).unwrap(),
    };
    batch.batch_digest = canonical_batch_digest(&batch);
    batch
}

fn main_session_transfer_batch(
    seed: u8,
    cursor_before: SurfaceCursor,
    fence: SurfaceOperationFence,
    background_fence: SurfaceBackgroundFence,
    task: SurfaceTask,
) -> SurfaceCommitBatch {
    let durable_revision = match cursor_before.source_revision {
        CursorSourceRevision::Recorded { durable_revision } => {
            DurableRevision::try_new(durable_revision.get() + 1).unwrap()
        }
        CursorSourceRevision::Ephemeral { .. } => panic!("test requires recorded cursor"),
    };
    let class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        durable_revision,
        commit_id: SurfaceCommitId::try_from_bytes(uuid(seed)).unwrap(),
    };
    let task_id = task.task_id.clone();
    let events = vec![
        SurfaceEventEnvelope {
            ordinal: 0,
            event_id: SurfaceEventId::try_from_bytes(uuid(seed.wrapping_add(1))).unwrap(),
            commit_class: class.clone(),
            scope: SurfaceScope::Generation {
                fence: fence.clone(),
            },
            event: SurfaceEvent::Operation(OperationPatch::GenerationTransferred {
                fence,
                background_fence,
                task_id: Some(task_id),
            }),
        },
        SurfaceEventEnvelope {
            ordinal: 1,
            event_id: SurfaceEventId::try_from_bytes(uuid(seed.wrapping_add(2))).unwrap(),
            commit_class: class.clone(),
            scope: SurfaceScope::Thread,
            event: SurfaceEvent::Task(TaskPatch::Upserted {
                expected_revision: None,
                task,
            }),
        },
    ];
    let mut batch = SurfaceCommitBatch {
        cursor_after: SurfaceCursor {
            next_seq: SequenceNumber::new(cursor_before.next_seq.get() + 2),
            source_revision: CursorSourceRevision::Recorded { durable_revision },
            ..cursor_before.clone()
        },
        cursor_before,
        commit_class: class,
        event_count: 2,
        batch_digest: digest(0),
        events: NonEmptyVec::try_new(events).unwrap(),
    };
    batch.batch_digest = canonical_batch_digest(&batch);
    batch
}

fn with_owner_epoch(mut batch: SurfaceCommitBatch, owner_epoch: u64) -> SurfaceCommitBatch {
    let CommitClass::Recorded {
        durable_revision,
        commit_id,
        ..
    } = batch.commit_class.clone()
    else {
        panic!("test requires recorded batch");
    };
    let commit_class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(owner_epoch),
        durable_revision,
        commit_id,
    };
    batch.commit_class = commit_class.clone();
    batch.events = NonEmptyVec::try_new(
        batch
            .events
            .as_slice()
            .iter()
            .cloned()
            .map(|mut event| {
                event.commit_class = commit_class.clone();
                event
            })
            .collect(),
    )
    .unwrap();
    batch.batch_digest = canonical_batch_digest(&batch);
    batch
}

#[derive(Clone, Copy)]
enum Fault {
    Partial,
    Failed,
}

#[derive(Debug, Eq, PartialEq)]
enum LedgerEvent {
    Append,
    Checkpoint,
}

#[derive(Default)]
struct FakeLedger {
    fault: Option<Fault>,
    events: Vec<LedgerEvent>,
    receipt: Option<SurfaceBatchReceipt>,
}

impl SurfaceCommitLedger for FakeLedger {
    fn append_complete_batch(
        &mut self,
        batch: &SurfaceCommitBatch,
    ) -> Result<SurfaceBatchReceipt, SurfaceLedgerError> {
        self.events.push(LedgerEvent::Append);
        if matches!(self.fault, Some(Fault::Partial)) {
            return Err(SurfaceLedgerError::PartialAppend);
        }
        if matches!(self.fault, Some(Fault::Failed)) {
            return Err(SurfaceLedgerError::AppendFailed);
        }
        let (commit_id, durable_revision) = match &batch.commit_class {
            CommitClass::Recorded {
                commit_id,
                durable_revision,
                ..
            } => (commit_id.clone(), *durable_revision),
            CommitClass::Ephemeral { .. } => unreachable!(),
        };
        let receipt = DurableBatchReceipt {
            commit_id,
            durable_revision,
            event_count: batch.event_count,
            batch_digest: batch.batch_digest.clone(),
            cursor_after: batch.cursor_after.clone(),
        };
        let receipt = SurfaceBatchReceipt::Recorded(receipt);
        self.receipt = Some(receipt.clone());
        Ok(receipt)
    }

    fn checkpoint(&mut self, _receipt: &SurfaceBatchReceipt) -> Result<(), SurfaceLedgerError> {
        self.events.push(LedgerEvent::Checkpoint);
        Ok(())
    }

    fn probe_commit(&self, id: &SurfaceCommitId, digest: &Sha256Digest) -> CommitProbe {
        let Some(receipt) = self.receipt.clone() else {
            return CommitProbe::Absent;
        };
        let (stored_id, stored_digest) = match &receipt {
            SurfaceBatchReceipt::Recorded(receipt) => (&receipt.commit_id, &receipt.batch_digest),
            SurfaceBatchReceipt::Ephemeral(receipt) => (&receipt.commit_id, &receipt.batch_digest),
        };
        if stored_id != id {
            CommitProbe::Absent
        } else if stored_digest == digest {
            CommitProbe::Present(receipt)
        } else {
            CommitProbe::Conflict
        }
    }
}

#[test]
fn commit_controller_trace_equivalence() {
    let (_owner_dir, owner) = test_owner_lease();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        FakeLedger {
            fault: Some(Fault::Failed),
            ..FakeLedger::default()
        },
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    let prepared = batch(9);
    let commit_id = match &prepared.commit_class {
        CommitClass::Recorded { commit_id, .. } => commit_id.clone(),
        CommitClass::Ephemeral { .. } => unreachable!(),
    };

    assert!(coordinator.commit_actor_batch(&prepared).is_err());
    assert_eq!(coordinator.state().snapshot().cursor.next_seq.get(), 0);
    assert_eq!(coordinator.ledger().events, [LedgerEvent::Append]);

    coordinator.ledger_mut().fault = None;
    coordinator.commit_actor_batch(&prepared).unwrap();
    assert_eq!(
        coordinator.ledger().events,
        [
            LedgerEvent::Append,
            LedgerEvent::Append,
            LedgerEvent::Checkpoint
        ]
    );
    assert_eq!(coordinator.state().snapshot().cursor, prepared.cursor_after);
    assert!(matches!(
        coordinator
            .ledger()
            .probe_commit(&commit_id, &prepared.batch_digest),
        CommitProbe::Present(_)
    ));
    let unrelated = batch(10);
    let unrelated_id = match &unrelated.commit_class {
        CommitClass::Recorded { commit_id, .. } => commit_id,
        CommitClass::Ephemeral { .. } => unreachable!(),
    };
    assert_eq!(
        coordinator
            .ledger()
            .probe_commit(unrelated_id, &unrelated.batch_digest),
        CommitProbe::Absent
    );
}

#[test]
fn durable_append_precedes_materialization() {
    let (_owner_dir, owner) = test_owner_lease();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        FakeLedger::default(),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator.commit_actor_batch(&batch(10)).unwrap();
    assert_eq!(
        coordinator.ledger().events,
        [LedgerEvent::Append, LedgerEvent::Checkpoint]
    );
    assert_eq!(coordinator.state().snapshot().cursor.next_seq.get(), 1);
}

#[test]
fn recovery_terminalizes_interrupted_capability_calls_without_replay() {
    let mut recovered_snapshot = snapshot();
    let mut operation = requested_operation(180);
    operation.intent.kind = OperationKind::UserTurn;
    operation.phase = OperationPhase::Admitted;
    operation.ready_for_admission = true;
    let turn_id = SurfaceTurnId::new();
    let fence = SurfaceOperationFence {
        thread_id: recovered_snapshot.thread.thread_id.clone(),
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        operation_id: operation.operation_id.clone(),
        generation_id: SurfaceGenerationId::new(0),
    };
    operation.generations.push(GenerationRecord {
        fence: fence.clone(),
        logical_turn_id: turn_id.clone(),
        input: GenerationInputState::NotApplicable,
        predecessor: None,
        attempt: GenerationAttempt::Initial,
        goal_identity: None,
        replayability: operation.intent.initial_replayability.clone(),
        required_capabilities: Default::default(),
        capability_fingerprint: operation.intent.capability_fingerprint.clone(),
        phase: GenerationPhase::Started,
        started_witness: None,
        stop_reason: None,
    });
    let tool_call_id = SurfaceToolCallId::try_new("read-recovery").unwrap();
    let second_tool_call_id = SurfaceToolCallId::try_new("write-recovery").unwrap();
    let terminal_tool_call_id = SurfaceToolCallId::try_new("terminal-create-recovery").unwrap();
    let live_terminal_tool_call_id = SurfaceToolCallId::try_new("terminal-live-recovery").unwrap();
    let call = |seed, owning_tool_call_id: &SurfaceToolCallId, kind, state| SurfaceCapabilityCall {
        call_id: SurfaceCapabilityCallId::try_from_bytes(uuid(seed)).unwrap(),
        acp_session_id: NonEmptyText::try_new("session-recovery").unwrap(),
        fence: fence.clone(),
        capability_revision: CapabilityRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        kind,
        arguments_digest: digest(seed),
        owning_tool_call_id: owning_tool_call_id.clone(),
        state,
    };
    recovered_snapshot.foreground_operation = Some(operation.clone());
    recovered_snapshot.tools.push(SurfaceToolView {
        request: SurfaceToolRequest {
            tool_call_id: tool_call_id.clone(),
            source_response_id: Some(UuidV7::try_from_bytes(uuid(183)).unwrap()),
            turn_id,
            name: NonEmptyText::try_new("read_file").unwrap(),
            action: SurfaceToolAction::Read,
            target: Some(DisplayText::new("/tmp/input.txt")),
            raw_arguments: DisplayText::new(r#"{"path":"/tmp/input.txt"}"#),
            arguments_digest: digest(184),
        },
        state: SurfaceToolViewState::Running,
        invocation_started: None,
        arguments_bytes: ByteCount::new(30),
        output_bytes: ByteCount::new(0),
        streamed_output: DisplayText::new(""),
        streamed_output_truncated: false,
        result: None,
        capability_calls: vec![
            call(
                185,
                &tool_call_id,
                SurfaceCapabilityCallKind::ReadTextFile,
                SurfaceCapabilityCallState::Prepared,
            ),
            call(
                186,
                &tool_call_id,
                SurfaceCapabilityCallKind::ReadTextFile,
                SurfaceCapabilityCallState::WrittenAwaitingResponse,
            ),
            call(
                190,
                &tool_call_id,
                SurfaceCapabilityCallKind::WriteTextFile,
                SurfaceCapabilityCallState::Prepared,
            ),
            call(
                191,
                &tool_call_id,
                SurfaceCapabilityCallKind::WriteTextFile,
                SurfaceCapabilityCallState::DeliveryPossible,
            ),
            call(
                207,
                &tool_call_id,
                SurfaceCapabilityCallKind::TerminalOutput,
                SurfaceCapabilityCallState::Prepared,
            ),
            call(
                208,
                &tool_call_id,
                SurfaceCapabilityCallKind::TerminalWaitForExit,
                SurfaceCapabilityCallState::WrittenAwaitingResponse,
            ),
        ],
        terminal_leases: Vec::new(),
    });
    let live_terminal_id = SurfaceRemoteTerminalId::try_new("terminal-live").unwrap();
    let second_live_terminal_id = SurfaceRemoteTerminalId::try_new("terminal-live-second").unwrap();
    let live_create_call = call(
        198,
        &live_terminal_tool_call_id,
        SurfaceCapabilityCallKind::TerminalCreate,
        SurfaceCapabilityCallState::Completed {
            result: CapabilityCallResult::TerminalCreated {
                terminal_id: live_terminal_id.clone(),
            },
            response_digest: digest(199),
        },
    );
    let second_live_create_call = call(
        202,
        &live_terminal_tool_call_id,
        SurfaceCapabilityCallKind::TerminalCreate,
        SurfaceCapabilityCallState::Completed {
            result: CapabilityCallResult::TerminalCreated {
                terminal_id: second_live_terminal_id.clone(),
            },
            response_digest: digest(203),
        },
    );
    let live_kill_call = SurfaceCapabilityCall {
        call_id: SurfaceCapabilityCallId::try_from_bytes(uuid(204)).unwrap(),
        acp_session_id: NonEmptyText::try_new("session-recovery").unwrap(),
        fence: fence.clone(),
        capability_revision: CapabilityRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        kind: SurfaceCapabilityCallKind::TerminalKill,
        arguments_digest: Sha256Digest::new(
            sha2::Sha256::digest(live_terminal_id.as_str().as_bytes()).into(),
        ),
        owning_tool_call_id: live_terminal_tool_call_id.clone(),
        state: SurfaceCapabilityCallState::DeliveryPossible,
    };
    let second_live_release_call = SurfaceCapabilityCall {
        call_id: SurfaceCapabilityCallId::try_from_bytes(uuid(205)).unwrap(),
        acp_session_id: NonEmptyText::try_new("session-recovery").unwrap(),
        fence: fence.clone(),
        capability_revision: CapabilityRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        kind: SurfaceCapabilityCallKind::TerminalRelease,
        arguments_digest: Sha256Digest::new(
            sha2::Sha256::digest(second_live_terminal_id.as_str().as_bytes()).into(),
        ),
        owning_tool_call_id: live_terminal_tool_call_id.clone(),
        state: SurfaceCapabilityCallState::WrittenAwaitingResponse,
    };
    recovered_snapshot.tools.push(SurfaceToolView {
        request: SurfaceToolRequest {
            tool_call_id: live_terminal_tool_call_id.clone(),
            source_response_id: Some(UuidV7::try_from_bytes(uuid(200)).unwrap()),
            turn_id: operation.generations[0].logical_turn_id.clone(),
            name: NonEmptyText::try_new("bash").unwrap(),
            action: SurfaceToolAction::Shell,
            target: Some(DisplayText::new("sleep")),
            raw_arguments: DisplayText::new(r#"{"command":"sleep","args":["10"]}"#),
            arguments_digest: digest(201),
        },
        state: SurfaceToolViewState::Running,
        invocation_started: None,
        arguments_bytes: ByteCount::new(38),
        output_bytes: ByteCount::new(0),
        streamed_output: DisplayText::new(""),
        streamed_output_truncated: false,
        result: None,
        capability_calls: vec![
            live_create_call.clone(),
            second_live_create_call.clone(),
            live_kill_call,
            second_live_release_call,
        ],
        terminal_leases: vec![
            SurfaceRemoteTerminalLease {
                lease_id: UuidV7::try_from_bytes(*live_create_call.call_id.as_bytes()).unwrap(),
                owning_tool_call_id: live_terminal_tool_call_id.clone(),
                state: SurfaceRemoteTerminalLeaseState::KillPending {
                    terminal_id: live_terminal_id.clone(),
                    owner_fence: fence.clone(),
                },
            },
            SurfaceRemoteTerminalLease {
                lease_id: UuidV7::try_from_bytes(*second_live_create_call.call_id.as_bytes())
                    .unwrap(),
                owning_tool_call_id: live_terminal_tool_call_id.clone(),
                state: SurfaceRemoteTerminalLeaseState::ReleasePending {
                    terminal_id: second_live_terminal_id.clone(),
                    owner_fence: fence.clone(),
                },
            },
        ],
    });
    recovered_snapshot.tools.push(SurfaceToolView {
        request: SurfaceToolRequest {
            tool_call_id: terminal_tool_call_id.clone(),
            source_response_id: Some(UuidV7::try_from_bytes(uuid(195)).unwrap()),
            turn_id: operation.generations[0].logical_turn_id.clone(),
            name: NonEmptyText::try_new("bash").unwrap(),
            action: SurfaceToolAction::Shell,
            target: Some(DisplayText::new("printf")),
            raw_arguments: DisplayText::new(r#"{"command":"printf","args":["hello"]}"#),
            arguments_digest: digest(196),
        },
        state: SurfaceToolViewState::Running,
        invocation_started: None,
        arguments_bytes: ByteCount::new(39),
        output_bytes: ByteCount::new(0),
        streamed_output: DisplayText::new(""),
        streamed_output_truncated: false,
        result: None,
        capability_calls: vec![
            call(
                197,
                &terminal_tool_call_id,
                SurfaceCapabilityCallKind::TerminalCreate,
                SurfaceCapabilityCallState::DeliveryPossible,
            ),
            call(
                206,
                &terminal_tool_call_id,
                SurfaceCapabilityCallKind::TerminalCreate,
                SurfaceCapabilityCallState::WrittenAwaitingResponse,
            ),
        ],
        terminal_leases: Vec::new(),
    });
    recovered_snapshot.tools.push(SurfaceToolView {
        request: SurfaceToolRequest {
            tool_call_id: second_tool_call_id.clone(),
            source_response_id: Some(UuidV7::try_from_bytes(uuid(193)).unwrap()),
            turn_id: operation.generations[0].logical_turn_id.clone(),
            name: NonEmptyText::try_new("write_file").unwrap(),
            action: SurfaceToolAction::Write,
            target: Some(DisplayText::new("/tmp/output.txt")),
            raw_arguments: DisplayText::new(r#"{"path":"/tmp/output.txt","content":"recovered"}"#),
            arguments_digest: digest(194),
        },
        state: SurfaceToolViewState::Running,
        invocation_started: None,
        arguments_bytes: ByteCount::new(52),
        output_bytes: ByteCount::new(0),
        streamed_output: DisplayText::new(""),
        streamed_output_truncated: false,
        result: None,
        capability_calls: vec![call(
            192,
            &second_tool_call_id,
            SurfaceCapabilityCallKind::WriteTextFile,
            SurfaceCapabilityCallState::WrittenAwaitingResponse,
        )],
        terminal_leases: Vec::new(),
    });
    let owner_dir = tempfile::tempdir().unwrap();
    let ledger_dir = tempfile::tempdir().unwrap();
    let lock_path = owner_dir.path().join("thread.lock");
    let epoch_path = owner_dir.path().join("thread.epoch");
    let first_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &FakeClock {
            clock_id: HostMonotonicClockId::try_from_bytes(uuid(187)).unwrap(),
            tick: 1,
            wall_ms: 1,
        },
    )
    .unwrap();
    drop(first_owner);
    let owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &FakeClock {
            clock_id: HostMonotonicClockId::try_from_bytes(uuid(188)).unwrap(),
            tick: 2,
            wall_ms: 2,
        },
    )
    .unwrap();
    let ledger_path = ledger_dir.path().join("capability-recovery.jsonl");
    let mut coordinator = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(recovered_snapshot),
        &owner,
    )
    .unwrap();
    let cause = MaterializationCause::ColdOwnerTakeover {
        new_incarnation: SurfaceIncarnation::try_from_bytes(uuid(189)).unwrap(),
        new_owner_epoch: ThreadOwnerEpoch::new(2),
    };

    coordinator
        .recover_interrupted_capability_calls(&operation.operation_id, &cause)
        .unwrap();

    let calls = coordinator
        .state()
        .snapshot()
        .tools
        .iter()
        .flat_map(|tool| tool.capability_calls.iter())
        .collect::<Vec<_>>();
    assert!(matches!(
        calls[0].state,
        SurfaceCapabilityCallState::FailedBeforeWrite { .. }
    ));
    assert!(matches!(
        calls[1].state,
        SurfaceCapabilityCallState::ObservationUnavailable { .. }
    ));
    assert!(matches!(
        calls[2].state,
        SurfaceCapabilityCallState::FailedBeforeWrite { .. }
    ));
    assert!(matches!(
        calls[3].state,
        SurfaceCapabilityCallState::ExternalEffectAmbiguous {
            effect_kind: ExternalEffectKind::FileWrite,
            ..
        }
    ));
    let recovered_terminal_output = calls
        .iter()
        .find(|call| call.call_id == SurfaceCapabilityCallId::try_from_bytes(uuid(207)).unwrap())
        .expect("recovered terminal output call");
    assert!(matches!(
        recovered_terminal_output.state,
        SurfaceCapabilityCallState::FailedBeforeWrite { .. }
    ));
    let recovered_terminal_wait = calls
        .iter()
        .find(|call| call.call_id == SurfaceCapabilityCallId::try_from_bytes(uuid(208)).unwrap())
        .expect("recovered terminal wait call");
    assert!(matches!(
        recovered_terminal_wait.state,
        SurfaceCapabilityCallState::ObservationUnavailable { .. }
    ));
    let recovered_terminal_create = calls
        .iter()
        .find(|call| call.call_id == SurfaceCapabilityCallId::try_from_bytes(uuid(197)).unwrap())
        .expect("recovered terminal create call");
    assert!(matches!(
        recovered_terminal_create.state,
        SurfaceCapabilityCallState::ExternalEffectAmbiguous {
            effect_kind: ExternalEffectKind::TerminalCreate,
            ..
        }
    ));
    let recovered_write = calls
        .iter()
        .find(|call| call.call_id == SurfaceCapabilityCallId::try_from_bytes(uuid(192)).unwrap())
        .expect("recovered write call");
    assert!(matches!(
        recovered_write.state,
        SurfaceCapabilityCallState::ExternalEffectAmbiguous {
            effect_kind: ExternalEffectKind::FileWrite,
            ..
        }
    ));
    let live_terminal_tool = coordinator
        .state()
        .snapshot()
        .tools
        .iter()
        .find(|tool| tool.request.tool_call_id == live_terminal_tool_call_id)
        .expect("live terminal recovery tool");
    assert!(live_terminal_tool.capability_calls.iter().any(|call| {
        matches!(
            call.state,
            SurfaceCapabilityCallState::ExternalEffectAmbiguous {
                effect_kind: ExternalEffectKind::TerminalKill,
                ..
            }
        )
    }));
    assert_eq!(live_terminal_tool.terminal_leases.len(), 2);
    assert!(live_terminal_tool.terminal_leases.iter().any(|lease| {
        matches!(
            &lease.state,
            SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                terminal_id: Some(terminal_id),
                ..
            } if terminal_id == &live_terminal_id
        )
    }));
    assert!(live_terminal_tool.terminal_leases.iter().any(|lease| {
        matches!(
            &lease.state,
            SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                terminal_id: Some(terminal_id),
                ..
            } if terminal_id == &second_live_terminal_id
        )
    }));
    assert!(matches!(
        live_terminal_tool.state,
        SurfaceToolViewState::Completed
    ));
    assert!(matches!(
        live_terminal_tool
            .result
            .as_ref()
            .map(|result| result.terminal.kind),
        Some(SurfaceToolResultKind::ExternalEffectAmbiguous)
    ));
    let terminal_tool = coordinator
        .state()
        .snapshot()
        .tools
        .iter()
        .find(|tool| tool.request.tool_call_id == terminal_tool_call_id)
        .expect("terminal create recovery tool");
    assert_eq!(terminal_tool.terminal_leases.len(), 2);
    assert!(terminal_tool.terminal_leases.iter().all(|lease| matches!(
        lease.state,
        SurfaceRemoteTerminalLeaseState::IdentityUnknown { .. }
    )));
    assert!(coordinator.state().snapshot().tools.iter().all(|tool| {
        matches!(
            tool.result.as_ref().map(|result| result.terminal.kind),
            Some(SurfaceToolResultKind::ExternalEffectAmbiguous)
        )
    }));
    loop {
        let before = coordinator.state().snapshot().cursor.clone();
        let action = coordinator
            .recover_operation(&operation.operation_id, &cause)
            .unwrap();
        if action == RecoveryAction::NoOp {
            break;
        }
        assert_ne!(
            coordinator.state().snapshot().cursor,
            before,
            "recovery action must advance durable state"
        );
    }
    let terminal = coordinator
        .state()
        .snapshot()
        .operation_history
        .iter()
        .find(|record| record.operation_id == operation.operation_id)
        .and_then(|record| record.terminal.as_ref())
        .expect("recovered operation terminal");
    assert!(matches!(
        &terminal.terminal,
        OperationTerminal::Failed {
            class: FailureClass::RemoteResourceCleanupAmbiguous,
            ..
        }
    ));
    assert!(
        std::fs::metadata(ledger_path).unwrap().len() > 0,
        "restart recovery must durably settle calls without redispatch"
    );
}

#[test]
fn publisher_permit_is_validated_before_reducer_or_wal() {
    let (_owner_dir, owner) = test_owner_lease();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        FakeLedger::default(),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    let stale = actor_permit(cursor(0).thread_id, 1);
    assert!(matches!(
        coordinator.commit_batch(&stale, &batch(11)),
        Err(SurfaceCommitError::StalePublisherPermit)
    ));
    assert!(coordinator.ledger().events.is_empty());
    assert_eq!(coordinator.state().snapshot().cursor.next_seq.get(), 0);
}

#[test]
fn coordinator_revalidates_live_owner_lease_before_every_write() {
    let dir = tempfile::tempdir().unwrap();
    let epoch_path = dir.path().join("thread.epoch");
    let clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(9)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let owner = ExclusiveOwnerLease::acquire_thread(
        dir.path().join("thread.lock"),
        &epoch_path,
        cursor(0).thread_id,
        &clock,
    )
    .unwrap();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        FakeLedger::default(),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    std::fs::write(&epoch_path, "2\n").unwrap();

    assert!(matches!(
        coordinator.commit_actor_batch(&batch(11)),
        Err(SurfaceCommitError::StaleOwnerEpoch)
    ));
    assert!(coordinator.ledger().events.is_empty());
}

#[test]
fn coordinator_rejects_policy_or_different_thread_owner_lease() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(10)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let policy = ExclusiveOwnerLease::acquire(
        dir.path().join("policy.lock"),
        dir.path().join("policy.epoch"),
        OwnerLeaseKind::Policy,
        &clock,
    )
    .unwrap();
    assert!(
        RuntimeCommitCoordinator::new_with_owner_lease(
            FakeLedger::default(),
            SurfaceReducerState::new(snapshot()),
            &policy,
        )
        .is_err()
    );

    let other_thread = ExclusiveOwnerLease::acquire_thread(
        dir.path().join("other.lock"),
        dir.path().join("other.epoch"),
        SurfaceThreadId::try_from_bytes([9; 16]).unwrap(),
        &clock,
    )
    .unwrap();
    assert!(
        RuntimeCommitCoordinator::new_with_owner_lease(
            FakeLedger::default(),
            SurfaceReducerState::new(snapshot()),
            &other_thread,
        )
        .is_err()
    );
}

#[test]
fn jsonl_ledger_recovers_exact_prepared_and_committed_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (_owner_dir, owner) = test_owner_lease();
    let path = dir.path().join("surface.jsonl");
    let ledger = JsonlSurfaceCommitLedger::new(&path, cursor(0));
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        ledger,
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    let batch = batch(12);
    coordinator.commit_actor_batch(&batch).unwrap();

    let lines = std::fs::read_to_string(&path).unwrap();
    let record_types = lines
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["type"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        record_types,
        [
            "runtime.surface_commit.prepared",
            "runtime.surface_commit.committed"
        ]
    );

    let commit_id = match &batch.commit_class {
        CommitClass::Recorded { commit_id, .. } => commit_id,
        CommitClass::Ephemeral { .. } => unreachable!(),
    };
    let reopened = JsonlSurfaceCommitLedger::new(&path, cursor(0));
    assert!(matches!(
        reopened.probe_commit(commit_id, &batch.batch_digest),
        CommitProbe::Present(SurfaceBatchReceipt::Recorded(receipt))
            if receipt.batch_digest == batch.batch_digest
    ));
}

#[test]
fn jsonl_ledger_repairs_torn_prepared_tail_before_durable_append() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = dir.path().join("complete-prepared.jsonl");
    let target_path = dir.path().join("torn-prepared.jsonl");
    let prepared_batch = batch(19);
    let mut source = JsonlSurfaceCommitLedger::new(&source_path, cursor(0));
    source.append_complete_batch(&prepared_batch).unwrap();
    let complete = std::fs::read(&source_path).unwrap();
    assert_eq!(complete.last(), Some(&b'\n'));
    std::fs::write(&target_path, &complete[..complete.len() / 2]).unwrap();

    let mut repaired = JsonlSurfaceCommitLedger::new(&target_path, cursor(0));
    repaired.append_complete_batch(&prepared_batch).unwrap();
    let recovered = repaired.recover_batches().unwrap();
    assert!(recovered.committed.is_empty());
    assert!(recovered.prepared.as_ref() == Some(&prepared_batch));
    let repaired_lines = std::fs::read_to_string(&target_path).unwrap();
    assert_eq!(repaired_lines.lines().count(), 1);
    serde_json::from_str::<serde_json::Value>(repaired_lines.lines().next().unwrap()).unwrap();
}

#[test]
fn reopened_ledger_rematerializes_full_batches_and_protects_prepared_range() {
    let dir = tempfile::tempdir().unwrap();
    let (_owner_dir, owner) = test_owner_lease();
    let committed_path = dir.path().join("committed.jsonl");
    let committed_batch = batch(16);
    let mut committed = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&committed_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    committed.commit_actor_batch(&committed_batch).unwrap();
    drop(committed);

    let reopened = JsonlSurfaceCommitLedger::new(&committed_path, cursor(0));
    let recovered = reopened.recover_batches().unwrap();
    assert_eq!(recovered.committed.len(), 1);
    assert!(recovered.committed[0] == committed_batch);
    let reopened =
        RuntimeCommitCoordinator::recover(reopened, SurfaceReducerState::new(snapshot()), &owner)
            .unwrap();
    assert_eq!(
        reopened.state().snapshot().cursor,
        committed_batch.cursor_after
    );

    let prepared_path = dir.path().join("prepared.jsonl");
    let prepared_batch = batch(17);
    let mut prepared = JsonlSurfaceCommitLedger::new(&prepared_path, cursor(0));
    prepared.append_complete_batch(&prepared_batch).unwrap();
    drop(prepared);

    let reopened = JsonlSurfaceCommitLedger::new(&prepared_path, cursor(0));
    let recovered = reopened.recover_batches().unwrap();
    assert!(recovered.prepared.as_ref() == Some(&prepared_batch));
    let mut reopened =
        RuntimeCommitCoordinator::recover(reopened, SurfaceReducerState::new(snapshot()), &owner)
            .unwrap();
    assert!(matches!(
        reopened.commit_actor_batch(&batch(18)),
        Err(SurfaceCommitError::CursorRangeAlreadyConsumed)
    ));
    reopened.commit_actor_batch(&prepared_batch).unwrap();
    assert_eq!(
        reopened.state().snapshot().cursor,
        prepared_batch.cursor_after
    );
}

#[test]
fn recover_retries_prepared_terminal_batch_before_classifying_operation() {
    let dir = tempfile::tempdir().unwrap();
    let (_owner_dir, owner) = test_owner_lease();
    let path = dir.path().join("prepared-terminal.jsonl");
    let operation = requested_operation(140);
    let operation_id = operation.operation_id.clone();
    let finalize_intent_id = SurfaceFinalizeIntentId::try_from_bytes(uuid(142)).unwrap();
    let terminal_commit_id = SurfaceCommitId::try_from_bytes(uuid(145)).unwrap();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator
        .commit_actor_batch(&operation_batch(141, operation))
        .unwrap();
    let finalizing = next_operation_batch(
        143,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::FinalizationStarted {
            operation_id: operation_id.clone(),
            finalize_intent_id: finalize_intent_id.clone(),
            terminal_commit_id: terminal_commit_id.clone(),
            selected_cause: OperationFinalizationCause::Reservation(
                ReservationFinalizerReason::RuntimeRestart,
            ),
            suspended_cause: None,
            expected_settlements: Vec::new(),
        }],
    );
    coordinator
        .commit_finalizer_batch(
            operation_id.clone(),
            finalize_intent_id.clone(),
            &finalizing,
        )
        .unwrap();
    let terminal = next_operation_batch(
        145,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::Terminal {
            record: OperationTerminalRecord {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal: OperationTerminal::NotAdmitted {
                    reason: NotAdmittedReason::RuntimeRestart,
                },
                usage: UsageTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_tokens: 0,
                    estimated_cost_usd_micros: 0,
                },
                source_diagnostic_digest: None,
                settlement_receipts: Vec::new(),
                committed_at: UnixMillis::new(0),
            },
        }],
    );
    assert!(matches!(
        terminal.commit_class,
        CommitClass::Recorded { ref commit_id, .. } if commit_id == &terminal_commit_id
    ));
    drop(coordinator);
    let mut ledger = JsonlSurfaceCommitLedger::new(&path, cursor(0));
    ledger.append_complete_batch(&terminal).unwrap();
    assert!(matches!(
        ledger.probe_commit(&terminal_commit_id, &terminal.batch_digest),
        CommitProbe::Prepared(_)
    ));
    drop(ledger);

    let reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    let recovered = reopened
        .state()
        .snapshot()
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == operation_id)
        .unwrap();
    assert!(matches!(recovered.phase, OperationPhase::Terminal));
    assert!(matches!(
        reopened
            .ledger()
            .probe_commit(&terminal_commit_id, &terminal.batch_digest),
        CommitProbe::Present(_)
    ));
    assert_eq!(
        reopened.recovery_action(&operation_id, &same_process_reset()),
        Some(RecoveryAction::NoOp)
    );
}

#[test]
fn later_successor_recovers_prepared_terminal_before_any_owner_transition() {
    let ledger_dir = tempfile::tempdir().unwrap();
    let owner_dir = tempfile::tempdir().unwrap();
    let ledger_path = ledger_dir.path().join("successor-prepared-terminal.jsonl");
    let lock_path = owner_dir.path().join("thread.lock");
    let epoch_path = owner_dir.path().join("thread.epoch");
    let first_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(146)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let first_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &first_clock,
    )
    .unwrap();
    assert_eq!(first_owner.owner_epoch(), 1);

    let operation = requested_operation(147);
    let operation_id = operation.operation_id.clone();
    let finalize_intent_id = SurfaceFinalizeIntentId::try_from_bytes(uuid(149)).unwrap();
    let terminal_commit_id = SurfaceCommitId::try_from_bytes(uuid(152)).unwrap();
    let mut first = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &first_owner,
    )
    .unwrap();
    first
        .commit_actor_batch(&operation_batch(148, operation))
        .unwrap();
    let finalizing = next_operation_batch(
        150,
        first.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::FinalizationStarted {
            operation_id: operation_id.clone(),
            finalize_intent_id: finalize_intent_id.clone(),
            terminal_commit_id: terminal_commit_id.clone(),
            selected_cause: OperationFinalizationCause::Reservation(
                ReservationFinalizerReason::RuntimeRestart,
            ),
            suspended_cause: None,
            expected_settlements: Vec::new(),
        }],
    );
    first
        .commit_finalizer_batch(
            operation_id.clone(),
            finalize_intent_id.clone(),
            &finalizing,
        )
        .unwrap();
    let terminal = next_operation_batch(
        152,
        first.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::Terminal {
            record: OperationTerminalRecord {
                operation_id: operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal: OperationTerminal::NotAdmitted {
                    reason: NotAdmittedReason::RuntimeRestart,
                },
                usage: UsageTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_tokens: 0,
                    estimated_cost_usd_micros: 0,
                },
                source_diagnostic_digest: None,
                settlement_receipts: Vec::new(),
                committed_at: UnixMillis::new(0),
            },
        }],
    );
    let historical_terminal = terminal.clone();
    let historical_digest = terminal.batch_digest.clone();
    let historical_class = terminal.commit_class.clone();
    first.ledger_mut().append_complete_batch(&terminal).unwrap();
    assert!(matches!(
        first
            .ledger()
            .probe_commit(&terminal_commit_id, &historical_digest),
        CommitProbe::Prepared(_)
    ));
    drop(first);
    drop(first_owner);

    let second_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(153)).unwrap(),
        tick: 2,
        wall_ms: 2,
    };
    let second_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &second_clock,
    )
    .unwrap();
    assert_eq!(second_owner.owner_epoch(), 2);
    let second = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &second_owner,
    )
    .unwrap();

    let recovered = second
        .state()
        .snapshot()
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == operation_id)
        .unwrap();
    assert!(matches!(recovered.phase, OperationPhase::Terminal));
    assert_eq!(
        recovered.terminal.as_ref().unwrap().finalize_intent_id,
        finalize_intent_id
    );
    assert!(matches!(
        second
            .ledger()
            .probe_commit(&terminal_commit_id, &historical_digest),
        CommitProbe::Present(_)
    ));
    let recovered_batches = second.ledger().recover_batches().unwrap();
    let recovered_terminal = recovered_batches.committed.last().unwrap();
    assert!(recovered_terminal == &historical_terminal);
    assert_eq!(recovered_terminal.commit_class, historical_class);
    assert_eq!(recovered_terminal.batch_digest, historical_digest);
    assert_eq!(second.state().snapshot().thread.owner_epoch.get(), 1);
    drop(second);
    drop(second_owner);

    let third_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(154)).unwrap(),
        tick: 3,
        wall_ms: 3,
    };
    let third_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &third_clock,
    )
    .unwrap();
    assert_eq!(third_owner.owner_epoch(), 3);
    let new_incarnation = SurfaceIncarnation::try_from_bytes(uuid(155)).unwrap();
    let cause = MaterializationCause::ColdOwnerTakeover {
        new_incarnation: new_incarnation.clone(),
        new_owner_epoch: ThreadOwnerEpoch::new(3),
    };
    let mut third = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &third_owner,
    )
    .unwrap();
    assert_eq!(
        third.recovery_action(&operation_id, &cause),
        Some(RecoveryAction::NoOp)
    );
    assert_eq!(
        third.recover_operation(&operation_id, &cause).unwrap(),
        RecoveryAction::NoOp
    );
    assert_eq!(third.state().snapshot().thread.owner_epoch.get(), 3);
    assert_eq!(third.state().snapshot().cursor.incarnation, new_incarnation);

    let mut stale = batch(156);
    let cursor_before = third.state().snapshot().cursor.clone();
    let durable_revision = match cursor_before.source_revision {
        CursorSourceRevision::Recorded { durable_revision } => {
            DurableRevision::try_new(durable_revision.get() + 1).unwrap()
        }
        CursorSourceRevision::Ephemeral { .. } => unreachable!(),
    };
    stale.cursor_before = cursor_before.clone();
    stale.cursor_after = SurfaceCursor {
        next_seq: SequenceNumber::new(cursor_before.next_seq.get() + 1),
        source_revision: CursorSourceRevision::Recorded { durable_revision },
        ..cursor_before
    };
    let stale_class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        durable_revision,
        commit_id: SurfaceCommitId::try_from_bytes(uuid(156)).unwrap(),
    };
    stale.commit_class = stale_class.clone();
    let mut stale_event = stale.events.as_slice()[0].clone();
    stale_event.commit_class = stale_class;
    stale.events = NonEmptyVec::try_new(vec![stale_event]).unwrap();
    stale.batch_digest = canonical_batch_digest(&stale);
    assert!(matches!(
        third.commit_actor_batch(&stale),
        Err(SurfaceCommitError::StaleOwnerEpoch)
    ));
}

#[test]
fn later_successor_recovers_prepared_batch_after_intermediate_owner_crash() {
    let ledger_dir = tempfile::tempdir().unwrap();
    let owner_dir = tempfile::tempdir().unwrap();
    let ledger_path = ledger_dir.path().join("multi-successor-prepared.jsonl");
    let lock_path = owner_dir.path().join("thread.lock");
    let epoch_path = owner_dir.path().join("thread.epoch");
    let first_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(157)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let first_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &first_clock,
    )
    .unwrap();
    assert_eq!(first_owner.owner_epoch(), 1);

    let operation = requested_operation(158);
    let operation_id = operation.operation_id.clone();
    let prepared_batch = operation_batch(159, operation);
    let prepared_digest = prepared_batch.batch_digest.clone();
    let prepared_commit_id = match &prepared_batch.commit_class {
        CommitClass::Recorded { commit_id, .. } => commit_id.clone(),
        CommitClass::Ephemeral { .. } => unreachable!(),
    };
    let mut ledger = JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0));
    ledger.append_complete_batch(&prepared_batch).unwrap();
    assert!(matches!(
        ledger.probe_commit(&prepared_commit_id, &prepared_digest),
        CommitProbe::Prepared(_)
    ));
    drop(ledger);
    drop(first_owner);

    let second_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(160)).unwrap(),
        tick: 2,
        wall_ms: 2,
    };
    let second_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &second_clock,
    )
    .unwrap();
    assert_eq!(second_owner.owner_epoch(), 2);
    drop(second_owner);

    let third_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(161)).unwrap(),
        tick: 3,
        wall_ms: 3,
    };
    let third_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &third_clock,
    )
    .unwrap();
    assert_eq!(third_owner.owner_epoch(), 3);
    let recovered = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &third_owner,
    )
    .unwrap();

    assert!(
        recovered
            .state()
            .snapshot()
            .queued_operations
            .iter()
            .any(|operation| operation.operation_id == operation_id)
    );
    assert!(matches!(
        recovered
            .ledger()
            .probe_commit(&prepared_commit_id, &prepared_digest),
        CommitProbe::Present(_)
    ));
}

#[test]
fn reopened_finalizing_reuses_persisted_finalizer_and_terminal_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (_owner_dir, owner) = test_owner_lease();
    let path = dir.path().join("cold-finalizing.jsonl");
    let operation = requested_operation(150);
    let operation_id = operation.operation_id.clone();
    let finalize_intent_id = SurfaceFinalizeIntentId::try_from_bytes(uuid(152)).unwrap();
    let terminal_commit_id = SurfaceCommitId::try_from_bytes(uuid(155)).unwrap();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator
        .commit_actor_batch(&operation_batch(151, operation))
        .unwrap();
    let finalizing = next_operation_batch(
        153,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::FinalizationStarted {
            operation_id: operation_id.clone(),
            finalize_intent_id: finalize_intent_id.clone(),
            terminal_commit_id: terminal_commit_id.clone(),
            selected_cause: OperationFinalizationCause::Reservation(
                ReservationFinalizerReason::RuntimeRestart,
            ),
            suspended_cause: None,
            expected_settlements: Vec::new(),
        }],
    );
    coordinator
        .commit_finalizer_batch(
            operation_id.clone(),
            finalize_intent_id.clone(),
            &finalizing,
        )
        .unwrap();
    drop(coordinator);

    let mut reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    assert_eq!(
        reopened.recovery_action(&operation_id, &same_process_reset()),
        Some(RecoveryAction::ReconcileOriginalFinalizer)
    );
    assert_eq!(
        reopened
            .recover_operation(&operation_id, &same_process_reset())
            .unwrap(),
        RecoveryAction::ReconcileOriginalFinalizer
    );
    let recovered = reopened
        .state()
        .snapshot()
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == operation_id)
        .unwrap();
    let terminal = recovered.terminal.as_ref().unwrap();
    assert!(matches!(recovered.phase, OperationPhase::Terminal));
    assert_eq!(terminal.finalize_intent_id, finalize_intent_id);
    assert!(matches!(
        terminal.terminal,
        OperationTerminal::NotAdmitted {
            reason: NotAdmittedReason::RuntimeRestart,
        }
    ));
    let recovered_batches = reopened.ledger().recover_batches().unwrap();
    let terminal_batch = recovered_batches.committed.last().unwrap();
    assert!(matches!(
        terminal_batch.commit_class,
        CommitClass::Recorded { ref commit_id, .. } if commit_id == &terminal_commit_id
    ));
    assert!(matches!(
        reopened
            .ledger()
            .probe_commit(&terminal_commit_id, &terminal_batch.batch_digest),
        CommitProbe::Present(_)
    ));
}

#[test]
fn cold_finalizing_reconciles_missing_settlements_before_terminal() {
    let ledger_dir = tempfile::tempdir().unwrap();
    let owner_dir = tempfile::tempdir().unwrap();
    let ledger_path = ledger_dir.path().join("cold-finalizing-settlements.jsonl");
    let lock_path = owner_dir.path().join("thread.lock");
    let epoch_path = owner_dir.path().join("thread.epoch");
    let first_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(156)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let first_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &first_clock,
    )
    .unwrap();
    let operation = requested_operation(157);
    let operation_id = operation.operation_id.clone();
    let finalize_intent_id = SurfaceFinalizeIntentId::try_from_bytes(uuid(159)).unwrap();
    let terminal_commit_id = SurfaceCommitId::try_from_bytes(uuid(162)).unwrap();
    let first_settlement = settlement(163);
    let second_settlement = settlement(164);
    let mut first = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &first_owner,
    )
    .unwrap();
    first
        .commit_actor_batch(&operation_batch(158, operation))
        .unwrap();
    let finalizing = next_operation_batch(
        160,
        first.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::FinalizationStarted {
            operation_id: operation_id.clone(),
            finalize_intent_id: finalize_intent_id.clone(),
            terminal_commit_id: terminal_commit_id.clone(),
            selected_cause: OperationFinalizationCause::Reservation(
                ReservationFinalizerReason::RuntimeRestart,
            ),
            suspended_cause: None,
            expected_settlements: vec![first_settlement.clone(), second_settlement.clone()],
        }],
    );
    first
        .commit_finalizer_batch(
            operation_id.clone(),
            finalize_intent_id.clone(),
            &finalizing,
        )
        .unwrap();
    drop(first);
    drop(first_owner);

    let second_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(165)).unwrap(),
        tick: 2,
        wall_ms: 2,
    };
    let second_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &second_clock,
    )
    .unwrap();
    let new_incarnation = SurfaceIncarnation::try_from_bytes(uuid(166)).unwrap();
    let cause = MaterializationCause::ColdOwnerTakeover {
        new_incarnation: new_incarnation.clone(),
        new_owner_epoch: ThreadOwnerEpoch::new(2),
    };
    let mut store = FakeSettlementStore::default();
    store.existing.insert(first_settlement.clone(), digest(163));
    let mut second = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &second_owner,
    )
    .unwrap();
    assert_eq!(
        second
            .recover_operation_with_settlement_store(&operation_id, &cause, &mut store)
            .unwrap(),
        RecoveryAction::ReconcileOriginalFinalizer
    );

    assert_eq!(store.applied, [second_settlement]);
    let terminal = second
        .state()
        .snapshot()
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == operation_id)
        .unwrap()
        .terminal
        .as_ref()
        .unwrap();
    assert_eq!(terminal.finalize_intent_id, finalize_intent_id);
    assert_eq!(
        terminal.settlement_receipts,
        vec![
            SurfaceSettlementReceipt {
                settlement_id: first_settlement,
                receipt_digest: digest(163),
            },
            SurfaceSettlementReceipt {
                settlement_id: settlement(164),
                receipt_digest: digest(99),
            },
        ]
    );
    let committed = second.ledger().recover_batches().unwrap().committed;
    assert!(matches!(
        committed.last().unwrap().commit_class,
        CommitClass::Recorded { ref commit_id, .. } if commit_id == &terminal_commit_id
    ));
    assert_eq!(second.state().snapshot().thread.owner_epoch.get(), 2);
    assert_eq!(
        second.state().snapshot().cursor.incarnation,
        new_incarnation
    );
}

#[test]
fn recover_retries_prepared_finalizer_settlement_with_exact_intent() {
    let dir = tempfile::tempdir().unwrap();
    let (_owner_dir, owner) = test_owner_lease();
    let path = dir.path().join("prepared-finalizer-settlement.jsonl");
    let operation = requested_operation(167);
    let operation_id = operation.operation_id.clone();
    let finalize_intent_id = SurfaceFinalizeIntentId::try_from_bytes(uuid(169)).unwrap();
    let terminal_commit_id = SurfaceCommitId::try_from_bytes(uuid(172)).unwrap();
    let settlement_id = settlement(173);
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator
        .commit_actor_batch(&operation_batch(168, operation))
        .unwrap();
    let finalizing = next_operation_batch(
        170,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::FinalizationStarted {
            operation_id: operation_id.clone(),
            finalize_intent_id: finalize_intent_id.clone(),
            terminal_commit_id,
            selected_cause: OperationFinalizationCause::Reservation(
                ReservationFinalizerReason::RuntimeRestart,
            ),
            suspended_cause: None,
            expected_settlements: vec![settlement_id.clone()],
        }],
    );
    coordinator
        .commit_finalizer_batch(
            operation_id.clone(),
            finalize_intent_id.clone(),
            &finalizing,
        )
        .unwrap();
    let settlement_batch = next_operation_batch(
        172,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::FinalizationSettlementRecorded {
            operation_id: operation_id.clone(),
            finalize_intent_id: finalize_intent_id.clone(),
            receipt: SurfaceSettlementReceipt {
                settlement_id: settlement_id.clone(),
                receipt_digest: digest(173),
            },
        }],
    );
    coordinator
        .ledger_mut()
        .append_complete_batch(&settlement_batch)
        .unwrap();
    drop(coordinator);

    let reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    let recovered = reopened
        .state()
        .snapshot()
        .queued_operations
        .iter()
        .find(|operation| operation.operation_id == operation_id)
        .unwrap();
    assert_eq!(
        recovered.finalization.as_ref().unwrap().settled,
        vec![SurfaceSettlementReceipt {
            settlement_id,
            receipt_digest: digest(173),
        }]
    );
}

#[test]
fn jsonl_control_ledger_durably_freezes_finalize_and_settlement_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.jsonl");
    let ledger = JsonlSurfaceControlLedger::new(&path);
    let intent_id = SurfaceFinalizeIntentId::try_from_bytes(uuid(13)).unwrap();
    let settlement_id = settlement(14);
    let intent =
        DurableFinalizeIntent::new(intent_id.clone(), vec![settlement_id.clone()]).unwrap();
    ledger.persist_owner_epoch(2).unwrap();
    ledger.persist_finalize_intent(&intent).unwrap();
    ledger
        .persist_settlement(&SurfaceSettlementReceipt {
            settlement_id,
            receipt_digest: digest(14),
        })
        .unwrap();

    let reopened = JsonlSurfaceControlLedger::new(&path);
    assert_eq!(
        reopened.load_finalize_intent(&intent_id).unwrap(),
        Some(intent.clone())
    );
    assert!(matches!(
        reopened.persist_finalize_intent(
            &DurableFinalizeIntent::new(intent_id, vec![settlement(16)]).unwrap()
        ),
        Err(SurfaceLedgerError::CommitIdentityConflict)
    ));
    let record_types = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["type"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        record_types,
        [
            "runtime.surface_owner_epoch",
            "runtime.surface_finalize_intent",
            "runtime.surface_settlement"
        ]
    );
}

#[test]
fn jsonl_shutdown_barrier_reopens_typed_closing_and_closed_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shutdown.jsonl");
    let control = JsonlSurfaceControlLedger::new(&path);
    let plan = ephemeral_shutdown_plan(20);
    let (barrier_id, session_closed) = match &plan {
        ShutdownBarrierPlan::CloseThread {
            barrier_id, thread, ..
        } => match thread {
            ShutdownThreadPlan::Ephemeral { session_closed, .. } => {
                (barrier_id.clone(), session_closed.clone())
            }
            ShutdownThreadPlan::Recorded { .. } => unreachable!(),
        },
        ShutdownBarrierPlan::ShutdownHost { .. } => unreachable!(),
    };
    let mut shutdown = ImmutableShutdownLedger::default();
    shutdown.record(plan.clone()).unwrap();
    assert!(!shutdown.signal_authorized());
    control.persist_shutdown_barrier(&mut shutdown).unwrap();
    assert!(shutdown.signal_authorized());

    let mut reopened = control.load_shutdown_barrier(&barrier_id).unwrap().unwrap();
    assert_eq!(reopened.plan(), Some(&plan));
    assert_eq!(reopened.retained_output(), None);

    let closed_cursor = SurfaceCursor {
        thread_id: session_closed.thread_id.clone(),
        next_seq: SequenceNumber::new(1),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(2).unwrap(),
        },
        ..cursor(0)
    };
    reopened
        .settle(MutationCommitAck::ThreadLocalCursor {
            cursor: closed_cursor.clone(),
            family: session_closed.family,
            event_id: session_closed.event_id,
            commit_class: CommitClass::Recorded {
                thread_owner_epoch: ThreadOwnerEpoch::new(1),
                durable_revision: DurableRevision::try_new(2).unwrap(),
                commit_id: session_closed.commit_id,
            },
        })
        .unwrap();
    let output = RetainedShutdownOutput::CloseThread {
        output: ClosedThreadReceipt::Ephemeral {
            thread_id: closed_cursor.thread_id.clone(),
            persistence: EphemeralThreadPersistence::EphemeralAttached,
            operation_terminals: Vec::new(),
            closed_cursor,
        },
    };
    reopened.close(output.clone()).unwrap();
    control.persist_shutdown_barrier(&mut reopened).unwrap();

    let closed = control.load_shutdown_barrier(&barrier_id).unwrap().unwrap();
    assert_eq!(closed.plan(), Some(&plan));
    assert_eq!(closed.retained_output(), Some(&output));
}

#[test]
fn partial_append_materializes_nothing_and_consumes_the_range() {
    let (_owner_dir, owner) = test_owner_lease();
    let mut ledger = FakeLedger::default();
    ledger.fault = Some(Fault::Partial);
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        ledger,
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    let prepared_batch = batch(20);
    assert!(matches!(
        coordinator.commit_actor_batch(&prepared_batch),
        Err(SurfaceCommitError::Ledger(
            SurfaceLedgerError::PartialAppend
        ))
    ));
    assert_eq!(coordinator.state().snapshot().cursor.next_seq.get(), 0);
    assert_eq!(coordinator.next_sequence(), 1);

    let different_batch = batch(21);
    assert!(matches!(
        coordinator.commit_actor_batch(&different_batch),
        Err(SurfaceCommitError::CursorRangeAlreadyConsumed)
    ));

    coordinator.ledger_mut().fault = None;
    coordinator.commit_actor_batch(&prepared_batch).unwrap();
    assert_eq!(coordinator.state().snapshot().cursor.next_seq.get(), 1);
}

#[test]
fn ordinary_append_failure_without_prepared_record_does_not_reserve_the_range() {
    let (_owner_dir, owner) = test_owner_lease();
    let mut ledger = FakeLedger::default();
    ledger.fault = Some(Fault::Failed);
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        ledger,
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    let attempted = batch(22);
    assert!(matches!(
        coordinator.commit_actor_batch(&attempted),
        Err(SurfaceCommitError::Ledger(SurfaceLedgerError::AppendFailed))
    ));
    assert_eq!(coordinator.next_sequence(), 0);
    assert_eq!(coordinator.state().snapshot().cursor.next_seq.get(), 0);

    coordinator.ledger_mut().fault = None;
    coordinator.commit_actor_batch(&attempted).unwrap();
    assert_eq!(coordinator.state().snapshot().cursor.next_seq.get(), 1);
}

#[test]
fn post_materialization_recovery_matches_all_fifteen_manifest_rows() {
    use RecoveryAction::*;
    use RecoveryMaterialization::*;
    use RecoveryReplayability::*;
    use RecoverySourcePhase::*;

    let rows = serde_json::from_str::<serde_json::Value>(MANIFEST).unwrap()
        ["post_materialization_recovery_matrix"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(rows.len(), 15);

    let cases = [
        (
            Requested,
            NotApplicable,
            ColdOwnerTakeover,
            FinalizeRequested,
        ),
        (Reserved, Replayable, ColdOwnerTakeover, StopAndSuspend),
        (
            Reserved,
            NonReplayableCurrent,
            SameProcessProjectionReset,
            StopAndSuspend,
        ),
        (
            Reserved,
            NonReplayableNotCurrent,
            ColdOwnerTakeover,
            StopAndFinalizeRecoveryAbort,
        ),
        (
            StartedOrTransferred {
                exact_terminal_interaction_unavailable: true,
            },
            NotApplicable,
            ColdOwnerTakeover,
            StopAndFinalizeClientCapabilityUnavailable,
        ),
        (
            StartedOrTransferred {
                exact_terminal_interaction_unavailable: false,
            },
            NotApplicable,
            ColdOwnerTakeover,
            StopAndFinalizeRuntimeRestart,
        ),
        (
            Suspended,
            Replayable,
            ColdOwnerTakeover,
            ExposeRecoveryRequired,
        ),
        (
            Suspended,
            NonReplayableCurrent,
            SameProcessProjectionReset,
            ExposeRecoveryRequired,
        ),
        (
            Suspended,
            NonReplayableNotCurrent,
            ColdOwnerTakeover,
            FinalizeRecoveryAbort,
        ),
        (
            ResumeStartingReserved,
            Replayable,
            ColdOwnerTakeover,
            StopAndRebaseSuspension,
        ),
        (
            ResumeStartingReserved,
            NonReplayableCurrent,
            SameProcessProjectionReset,
            StopAndRebaseSuspension,
        ),
        (
            ResumeStartingReserved,
            NonReplayableNotCurrent,
            ColdOwnerTakeover,
            StopAndFinalizeRecoveryAbort,
        ),
        (
            Finalizing,
            NotApplicable,
            ColdOwnerTakeover,
            ReconcileOriginalFinalizer,
        ),
        (
            FinalizingDegraded {
                cause: RecoveryDegradedCause::MissingFinalization,
            },
            NotApplicable,
            ColdOwnerTakeover,
            ExposeRetryFinalization,
        ),
        (Terminal, NotApplicable, ColdOwnerTakeover, NoOp),
    ];

    for (phase, replayability, materialization, expected) in cases {
        assert_eq!(
            decide_post_materialization_recovery(phase, replayability, materialization),
            expected
        );
    }
}

#[test]
fn reopened_coordinator_derives_requested_recovery_from_persisted_operation() {
    let dir = tempfile::tempdir().unwrap();
    let (_owner_dir, owner) = test_owner_lease();
    let path = dir.path().join("requested-recovery.jsonl");
    let operation = requested_operation(60);
    let operation_id = operation.operation_id.clone();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator
        .commit_actor_batch(&operation_batch(61, operation))
        .unwrap();
    drop(coordinator);

    let mut reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    assert_eq!(
        reopened.recovery_action(&operation_id, &same_process_reset()),
        Some(RecoveryAction::FinalizeRequested)
    );
    assert_eq!(
        reopened.recovery_action(
            &operation_id,
            &MaterializationCause::ColdOwnerTakeover {
                new_incarnation: SurfaceIncarnation::try_from_bytes(uuid(99)).unwrap(),
                new_owner_epoch: ThreadOwnerEpoch::new(2),
            },
        ),
        None
    );
    assert_eq!(
        reopened
            .recover_operation(&operation_id, &same_process_reset(),)
            .unwrap(),
        RecoveryAction::FinalizeRequested
    );
    assert!(matches!(
        reopened
            .state()
            .snapshot()
            .operation_history
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .and_then(|operation| operation.terminal.as_ref())
            .map(|record| &record.terminal),
        Some(OperationTerminal::NotAdmitted {
            reason: NotAdmittedReason::RuntimeRestart,
        })
    ));
    drop(reopened);

    let reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    assert_eq!(
        reopened.recovery_action(&operation_id, &same_process_reset()),
        Some(RecoveryAction::NoOp)
    );
}

#[test]
fn cold_owner_takeover_persists_new_epoch_and_incarnation_before_recovery() {
    let ledger_dir = tempfile::tempdir().unwrap();
    let owner_dir = tempfile::tempdir().unwrap();
    let ledger_path = ledger_dir.path().join("cold-owner-recovery.jsonl");
    let lock_path = owner_dir.path().join("thread.lock");
    let epoch_path = owner_dir.path().join("thread.epoch");
    let first_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(121)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let first_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &first_clock,
    )
    .unwrap();
    assert_eq!(first_owner.owner_epoch(), 1);
    let operation = requested_operation(122);
    let operation_id = operation.operation_id.clone();
    let mut first = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &first_owner,
    )
    .unwrap();
    first
        .commit_actor_batch(&operation_batch(123, operation))
        .unwrap();
    drop(first);
    drop(first_owner);

    let second_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(124)).unwrap(),
        tick: 2,
        wall_ms: 2,
    };
    let second_owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &second_clock,
    )
    .unwrap();
    assert_eq!(second_owner.owner_epoch(), 2);
    let new_incarnation = SurfaceIncarnation::try_from_bytes(uuid(125)).unwrap();
    let cause = MaterializationCause::ColdOwnerTakeover {
        new_incarnation: new_incarnation.clone(),
        new_owner_epoch: ThreadOwnerEpoch::new(2),
    };
    let mut second = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &second_owner,
    )
    .unwrap();
    assert_eq!(
        second.recovery_action(&operation_id, &cause),
        Some(RecoveryAction::FinalizeRequested)
    );
    assert_eq!(
        second.recover_operation(&operation_id, &cause).unwrap(),
        RecoveryAction::FinalizeRequested
    );
    assert_eq!(second.state().snapshot().thread.owner_epoch.get(), 2);
    assert_eq!(
        second.state().snapshot().cursor.incarnation,
        new_incarnation
    );

    let mut new_operation = requested_operation(127);
    new_operation.intent.initial_replayability = Replayability::NonReplayable {
        reason: NonReplayableReason::HistoryDisabled,
        live_capsule: LiveOperationCapsule::Available {
            incarnation: new_incarnation.clone(),
        },
    };
    let new_operation_id = new_operation.operation_id.clone();
    let requested = with_owner_epoch(
        next_operation_batch(
            128,
            second.state().snapshot().cursor.clone(),
            new_operation_id.clone(),
            vec![OperationPatch::Requested {
                operation: new_operation.clone(),
            }],
        ),
        2,
    );
    second.commit_actor_batch(&requested).unwrap();
    let logical_turn_id = SurfaceTurnId::new();
    let fence = SurfaceOperationFence {
        thread_id: cursor(0).thread_id,
        thread_owner_epoch: ThreadOwnerEpoch::new(2),
        operation_id: new_operation_id.clone(),
        generation_id: SurfaceGenerationId::new(0),
    };
    let admitted = with_owner_epoch(
        next_operation_batch(
            130,
            second.state().snapshot().cursor.clone(),
            new_operation_id.clone(),
            vec![OperationPatch::Admitted {
                operation_id: new_operation_id.clone(),
                logical_turn_id: logical_turn_id.clone(),
                input: AdmittedInput::NotApplicable,
                first_generation: GenerationRecord {
                    fence,
                    logical_turn_id,
                    input: GenerationInputState::NotApplicable,
                    predecessor: None,
                    attempt: GenerationAttempt::Initial,
                    goal_identity: None,
                    replayability: new_operation.intent.initial_replayability,
                    required_capabilities: Default::default(),
                    capability_fingerprint: new_operation.intent.capability_fingerprint,
                    phase: GenerationPhase::Reserved,
                    started_witness: None,
                    stop_reason: None,
                },
            }],
        ),
        2,
    );
    second.commit_actor_batch(&admitted).unwrap();
    assert_eq!(second.recovery_action(&new_operation_id, &cause), None);

    let mut stale_epoch_batch = batch(126);
    stale_epoch_batch.cursor_before = second.state().snapshot().cursor.clone();
    stale_epoch_batch.cursor_after = SurfaceCursor {
        next_seq: SequenceNumber::new(stale_epoch_batch.cursor_before.next_seq.get() + 1),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(
                match stale_epoch_batch.cursor_before.source_revision {
                    CursorSourceRevision::Recorded { durable_revision } => {
                        durable_revision.get() + 1
                    }
                    CursorSourceRevision::Ephemeral { .. } => unreachable!(),
                },
            )
            .unwrap(),
        },
        ..stale_epoch_batch.cursor_before.clone()
    };
    stale_epoch_batch.batch_digest = canonical_batch_digest(&stale_epoch_batch);
    assert!(matches!(
        second.commit_actor_batch(&stale_epoch_batch),
        Err(SurfaceCommitError::StaleOwnerEpoch)
    ));
    drop(second);

    let reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &second_owner,
    )
    .unwrap();
    assert_eq!(reopened.state().snapshot().thread.owner_epoch.get(), 2);
    assert_eq!(
        reopened.state().snapshot().cursor.incarnation,
        new_incarnation
    );
    assert_eq!(
        reopened.recovery_action(&operation_id, &cause),
        Some(RecoveryAction::NoOp)
    );
}

#[test]
fn caller_cannot_forge_cold_takeover_with_current_epoch_and_incarnation() {
    let ledger_dir = tempfile::tempdir().unwrap();
    let (_owner_dir, owner) = test_owner_lease();
    let mut operation = requested_operation(127);
    operation.intent.initial_replayability = Replayability::NonReplayable {
        reason: NonReplayableReason::HistoryDisabled,
        live_capsule: LiveOperationCapsule::Available {
            incarnation: cursor(0).incarnation,
        },
    };
    let operation_id = operation.operation_id.clone();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(ledger_dir.path().join("forged-cold.jsonl"), cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator
        .commit_actor_batch(&operation_batch(128, operation.clone()))
        .unwrap();
    let logical_turn_id = SurfaceTurnId::new();
    let fence = SurfaceOperationFence {
        thread_id: cursor(0).thread_id,
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        operation_id: operation_id.clone(),
        generation_id: SurfaceGenerationId::new(0),
    };
    let admitted = next_operation_batch(
        130,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::Admitted {
            operation_id: operation_id.clone(),
            logical_turn_id: logical_turn_id.clone(),
            input: AdmittedInput::NotApplicable,
            first_generation: GenerationRecord {
                fence,
                logical_turn_id,
                input: GenerationInputState::NotApplicable,
                predecessor: None,
                attempt: GenerationAttempt::Initial,
                goal_identity: None,
                replayability: operation.intent.initial_replayability,
                required_capabilities: Default::default(),
                capability_fingerprint: operation.intent.capability_fingerprint,
                phase: GenerationPhase::Reserved,
                started_witness: None,
                stop_reason: None,
            },
        }],
    );
    coordinator.commit_actor_batch(&admitted).unwrap();

    assert_eq!(
        coordinator.recovery_action(
            &operation_id,
            &MaterializationCause::SameProcessProjectionReset {
                retained_incarnation: cursor(0).incarnation,
            },
        ),
        Some(RecoveryAction::StopAndSuspend)
    );
    assert_eq!(
        coordinator.recovery_action(
            &operation_id,
            &MaterializationCause::ColdOwnerTakeover {
                new_incarnation: cursor(0).incarnation,
                new_owner_epoch: ThreadOwnerEpoch::new(1),
            },
        ),
        None
    );
}

#[test]
fn reopened_replayable_reserved_operation_commits_stop_and_recovery_suspension() {
    let dir = tempfile::tempdir().unwrap();
    let (_owner_dir, owner) = test_owner_lease();
    let ledger_path = dir.path().join("reserved-recovery.jsonl");
    let mut operation = requested_operation(70);
    operation.intent.initial_replayability = Replayability::Replayable {
        capsule_digest: digest(70),
        request: None,
        request_digest: None,
        cwd: path(),
        workspace_roots: vec![path()],
        settings_revision: SettingsRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        tool_schema_digest: digest(71),
    };
    let operation_id = operation.operation_id.clone();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator
        .commit_actor_batch(&operation_batch(71, operation.clone()))
        .unwrap();
    let logical_turn_id = SurfaceTurnId::new();
    let fence = SurfaceOperationFence {
        thread_id: cursor(0).thread_id,
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        operation_id: operation_id.clone(),
        generation_id: SurfaceGenerationId::new(0),
    };
    let generation = GenerationRecord {
        fence: fence.clone(),
        logical_turn_id: logical_turn_id.clone(),
        input: GenerationInputState::NotApplicable,
        predecessor: None,
        attempt: GenerationAttempt::Initial,
        goal_identity: None,
        replayability: operation.intent.initial_replayability.clone(),
        required_capabilities: Default::default(),
        capability_fingerprint: operation.intent.capability_fingerprint.clone(),
        phase: GenerationPhase::Reserved,
        started_witness: None,
        stop_reason: None,
    };
    let admitted = next_operation_batch(
        73,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::Admitted {
            operation_id: operation_id.clone(),
            logical_turn_id,
            input: AdmittedInput::NotApplicable,
            first_generation: generation,
        }],
    );
    coordinator.commit_actor_batch(&admitted).unwrap();
    drop(coordinator);

    let mut reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    assert_eq!(
        reopened
            .recover_operation(&operation_id, &same_process_reset(),)
            .unwrap(),
        RecoveryAction::StopAndSuspend
    );
    let recovered = reopened
        .state()
        .snapshot()
        .foreground_operation
        .as_ref()
        .unwrap();
    assert!(matches!(
        recovered.phase,
        OperationPhase::Suspended {
            cause: SuspensionCause::RecoveryRequired { .. }
        }
    ));
    assert!(matches!(
        recovered.generations.last().unwrap().stop_reason,
        Some(GenerationStopReason::NotStarted {
            reason: NotStartedReason::RuntimeRestart,
        })
    ));
}

#[test]
fn reopened_started_operation_commits_restart_stop_and_original_finalization() {
    let dir = tempfile::tempdir().unwrap();
    let (_owner_dir, owner) = test_owner_lease();
    let ledger_path = dir.path().join("started-recovery.jsonl");
    let operation = requested_operation(80);
    let operation_id = operation.operation_id.clone();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator
        .commit_actor_batch(&operation_batch(81, operation.clone()))
        .unwrap();
    let logical_turn_id = SurfaceTurnId::new();
    let fence = SurfaceOperationFence {
        thread_id: cursor(0).thread_id,
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        operation_id: operation_id.clone(),
        generation_id: SurfaceGenerationId::new(0),
    };
    let generation = GenerationRecord {
        fence: fence.clone(),
        logical_turn_id: logical_turn_id.clone(),
        input: GenerationInputState::NotApplicable,
        predecessor: None,
        attempt: GenerationAttempt::Initial,
        goal_identity: None,
        replayability: operation.intent.initial_replayability.clone(),
        required_capabilities: Default::default(),
        capability_fingerprint: operation.intent.capability_fingerprint.clone(),
        phase: GenerationPhase::Reserved,
        started_witness: None,
        stop_reason: None,
    };
    let admitted = next_operation_batch(
        83,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::Admitted {
            operation_id: operation_id.clone(),
            logical_turn_id,
            input: AdmittedInput::NotApplicable,
            first_generation: generation.clone(),
        }],
    );
    coordinator.commit_actor_batch(&admitted).unwrap();
    let started = next_operation_batch(
        85,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::GenerationStarted {
            fence: fence.clone(),
            witness: GenerationStartedWitness {
                started_commit_id: SurfaceCommitId::try_from_bytes(uuid(85)).unwrap(),
                settings_revision: SettingsRevision::try_new(1).unwrap(),
                policy_epoch: PolicyEpoch::try_new(1).unwrap(),
                durable_replayability_digest: canonical_replayability_digest(
                    &generation.replayability,
                ),
                capability_fingerprint: generation.capability_fingerprint,
            },
        }],
    );
    coordinator
        .commit_generation_batch(fence.clone(), &started)
        .unwrap();
    drop(coordinator);

    let mut reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    assert_eq!(
        reopened
            .recover_operation(&operation_id, &same_process_reset())
            .unwrap(),
        RecoveryAction::StopAndFinalizeRuntimeRestart
    );
    let recovered = reopened
        .state()
        .snapshot()
        .foreground_operation
        .as_ref()
        .unwrap();
    assert!(matches!(recovered.phase, OperationPhase::Finalizing { .. }));
    assert!(matches!(
        recovered.generations.last().unwrap().stop_reason,
        Some(GenerationStopReason::RuntimeRestart)
    ));
    assert!(matches!(
        recovered
            .finalization
            .as_ref()
            .map(|finalization| &finalization.selected_cause),
        Some(OperationFinalizationCause::GenerationStop(
            GenerationStopReason::RuntimeRestart
        ))
    ));
}

#[test]
fn reopened_transferred_operation_uses_exact_background_fence_for_recovery() {
    let ledger_dir = tempfile::tempdir().unwrap();
    let owner_dir = tempfile::tempdir().unwrap();
    let ledger_path = ledger_dir.path().join("transferred-recovery.jsonl");
    let lock_path = owner_dir.path().join("thread.lock");
    let epoch_path = owner_dir.path().join("thread.epoch");
    let first_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(129)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &first_clock,
    )
    .unwrap();
    let operation = requested_operation(130);
    let operation_id = operation.operation_id.clone();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator
        .commit_actor_batch(&operation_batch(131, operation.clone()))
        .unwrap();
    let logical_turn_id = SurfaceTurnId::new();
    let fence = SurfaceOperationFence {
        thread_id: cursor(0).thread_id,
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        operation_id: operation_id.clone(),
        generation_id: SurfaceGenerationId::new(0),
    };
    let generation = GenerationRecord {
        fence: fence.clone(),
        logical_turn_id: logical_turn_id.clone(),
        input: GenerationInputState::NotApplicable,
        predecessor: None,
        attempt: GenerationAttempt::Initial,
        goal_identity: None,
        replayability: operation.intent.initial_replayability.clone(),
        required_capabilities: Default::default(),
        capability_fingerprint: operation.intent.capability_fingerprint.clone(),
        phase: GenerationPhase::Reserved,
        started_witness: None,
        stop_reason: None,
    };
    let admitted = next_operation_batch(
        133,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::Admitted {
            operation_id: operation_id.clone(),
            logical_turn_id,
            input: AdmittedInput::NotApplicable,
            first_generation: generation.clone(),
        }],
    );
    coordinator.commit_actor_batch(&admitted).unwrap();
    let started = next_operation_batch(
        135,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::GenerationStarted {
            fence: fence.clone(),
            witness: GenerationStartedWitness {
                started_commit_id: SurfaceCommitId::try_from_bytes(uuid(135)).unwrap(),
                settings_revision: SettingsRevision::try_new(1).unwrap(),
                policy_epoch: PolicyEpoch::try_new(1).unwrap(),
                durable_replayability_digest: canonical_replayability_digest(
                    &generation.replayability,
                ),
                capability_fingerprint: generation.capability_fingerprint,
            },
        }],
    );
    coordinator
        .commit_generation_batch(fence.clone(), &started)
        .unwrap();
    let background_fence = SurfaceBackgroundFence {
        operation_fence: fence.clone(),
        background_owner_token: background_owner_token(),
    };
    let transferred = next_operation_batch(
        137,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::GenerationTransferred {
            fence: fence.clone(),
            background_fence: background_fence.clone(),
            task_id: None,
        }],
    );
    coordinator
        .commit_generation_batch(fence.clone(), &transferred)
        .unwrap();
    drop(coordinator);
    drop(owner);

    let second_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(139)).unwrap(),
        tick: 2,
        wall_ms: 2,
    };
    let owner = ExclusiveOwnerLease::acquire_thread(
        &lock_path,
        &epoch_path,
        cursor(0).thread_id,
        &second_clock,
    )
    .unwrap();
    let cause = MaterializationCause::ColdOwnerTakeover {
        new_incarnation: SurfaceIncarnation::try_from_bytes(uuid(140)).unwrap(),
        new_owner_epoch: ThreadOwnerEpoch::new(2),
    };
    let mut reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    assert_eq!(
        reopened.recover_operation(&operation_id, &cause).unwrap(),
        RecoveryAction::StopAndFinalizeRuntimeRestart
    );
    let recovered = reopened
        .state()
        .snapshot()
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == operation_id)
        .unwrap();
    assert!(matches!(recovered.phase, OperationPhase::Finalizing { .. }));
    assert!(matches!(
        recovered.generations.last().unwrap().stop_reason,
        Some(GenerationStopReason::RuntimeRestart)
    ));
    assert!(reopened.state().snapshot().background_operations[0].fence == background_fence);
    assert_eq!(
        reopened.recover_operation(&operation_id, &cause).unwrap(),
        RecoveryAction::ReconcileOriginalFinalizer
    );
    let terminal = reopened
        .state()
        .snapshot()
        .operation_history
        .iter()
        .find(|operation| operation.operation_id == operation_id)
        .unwrap();
    assert!(matches!(terminal.phase, OperationPhase::Terminal));
    assert!(reopened.state().snapshot().background_operations.is_empty());
}

#[test]
fn recover_retries_prepared_main_session_transfer_as_one_owner_batch() {
    let ledger_dir = tempfile::tempdir().unwrap();
    let owner_dir = tempfile::tempdir().unwrap();
    let ledger_path = ledger_dir
        .path()
        .join("prepared-main-session-transfer.jsonl");
    let lock_path = owner_dir.path().join("thread.lock");
    let epoch_path = owner_dir.path().join("thread.epoch");
    let clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(201)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let owner =
        ExclusiveOwnerLease::acquire_thread(&lock_path, &epoch_path, cursor(0).thread_id, &clock)
            .unwrap();
    let operation = requested_operation(202);
    let operation_id = operation.operation_id.clone();
    let mut coordinator = RuntimeCommitCoordinator::new_with_owner_lease(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    coordinator
        .commit_actor_batch(&operation_batch(203, operation.clone()))
        .unwrap();
    let logical_turn_id = SurfaceTurnId::new();
    let fence = SurfaceOperationFence {
        thread_id: cursor(0).thread_id,
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        operation_id: operation_id.clone(),
        generation_id: SurfaceGenerationId::new(0),
    };
    let generation = GenerationRecord {
        fence: fence.clone(),
        logical_turn_id: logical_turn_id.clone(),
        input: GenerationInputState::NotApplicable,
        predecessor: None,
        attempt: GenerationAttempt::Initial,
        goal_identity: None,
        replayability: operation.intent.initial_replayability.clone(),
        required_capabilities: Default::default(),
        capability_fingerprint: operation.intent.capability_fingerprint.clone(),
        phase: GenerationPhase::Reserved,
        started_witness: None,
        stop_reason: None,
    };
    let admitted = next_operation_batch(
        205,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::Admitted {
            operation_id: operation_id.clone(),
            logical_turn_id,
            input: AdmittedInput::NotApplicable,
            first_generation: generation.clone(),
        }],
    );
    coordinator.commit_actor_batch(&admitted).unwrap();
    let started = next_operation_batch(
        207,
        coordinator.state().snapshot().cursor.clone(),
        operation_id.clone(),
        vec![OperationPatch::GenerationStarted {
            fence: fence.clone(),
            witness: GenerationStartedWitness {
                started_commit_id: SurfaceCommitId::try_from_bytes(uuid(207)).unwrap(),
                settings_revision: SettingsRevision::try_new(1).unwrap(),
                policy_epoch: PolicyEpoch::try_new(1).unwrap(),
                durable_replayability_digest: canonical_replayability_digest(
                    &generation.replayability,
                ),
                capability_fingerprint: generation.capability_fingerprint,
            },
        }],
    );
    coordinator
        .commit_generation_batch(fence.clone(), &started)
        .unwrap();

    let background_fence = SurfaceBackgroundFence {
        operation_fence: fence.clone(),
        background_owner_token: background_owner_token(),
    };
    let task_id = SurfaceTaskId::try_new("prepared-main-session-task").unwrap();
    let transfer = main_session_transfer_batch(
        209,
        coordinator.state().snapshot().cursor.clone(),
        fence,
        background_fence.clone(),
        SurfaceTask {
            task_id: task_id.clone(),
            revision: TaskRevision::try_new(1).unwrap(),
            task_type: SurfaceTaskType::MainSession,
            status: SurfaceTaskStatus::Running,
            backgrounded: true,
            description: DisplayText::new("prepared main-session background"),
            created_at: UnixMillis::new(1),
            started_at: Some(UnixMillis::new(1)),
            completed_at: None,
            parent_operation: Some(operation_id.clone()),
            background_fence: Some(background_fence.clone()),
            workflow_run_id: None,
            subagent_id: None,
            pending_interaction_id: None,
            usage: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
        },
    );
    let transfer_commit_id = match &transfer.commit_class {
        CommitClass::Recorded { commit_id, .. } => commit_id.clone(),
        CommitClass::Ephemeral { .. } => unreachable!(),
    };
    coordinator
        .ledger_mut()
        .append_complete_batch(&transfer)
        .unwrap();
    assert!(matches!(
        coordinator
            .ledger()
            .probe_commit(&transfer_commit_id, &transfer.batch_digest),
        CommitProbe::Prepared(_)
    ));
    drop(coordinator);

    let reopened = RuntimeCommitCoordinator::recover(
        JsonlSurfaceCommitLedger::new(&ledger_path, cursor(0)),
        SurfaceReducerState::new(snapshot()),
        &owner,
    )
    .unwrap();
    assert!(matches!(
        reopened
            .ledger()
            .probe_commit(&transfer_commit_id, &transfer.batch_digest),
        CommitProbe::Present(_)
    ));
    let recovered_background = reopened
        .state()
        .snapshot()
        .background_operations
        .iter()
        .find(|background| background.operation_id == operation_id)
        .expect("prepared transfer restores the background owner");
    assert!(recovered_background.fence == background_fence);
    assert_eq!(recovered_background.task_id.as_ref(), Some(&task_id));
    let recovered_task = reopened
        .state()
        .snapshot()
        .tasks
        .iter()
        .find(|task| task.task_id == task_id)
        .expect("prepared transfer restores its main-session task");
    assert!(recovered_task.background_fence.as_ref() == Some(&background_fence));
    assert_eq!(
        recovered_task.parent_operation.as_ref(),
        Some(&operation_id)
    );
}

#[test]
fn same_process_reset_replays_historical_batch_byte_for_byte_but_cold_owner_is_not_current() {
    let historical = batch(90);
    let digest_before = historical.batch_digest.clone();
    let encoded_before = canonical_batch_encoded_bytes(&historical);
    let restored = match reduce_batch(
        SurfaceReduceMode::Rematerialization,
        &SurfaceReducerState::new(snapshot()),
        &historical,
    ) {
        SurfaceReduceResult::Applied { state } => state,
        _ => panic!("historical batch must replay through SurfaceReducer"),
    };
    assert_eq!(restored.snapshot().cursor, historical.cursor_after);
    assert_eq!(historical.batch_digest, digest_before);
    assert_eq!(canonical_batch_encoded_bytes(&historical), encoded_before);
    assert_eq!(
        decide_post_materialization_recovery(
            RecoverySourcePhase::Reserved,
            RecoveryReplayability::NonReplayableCurrent,
            RecoveryMaterialization::SameProcessProjectionReset,
        ),
        RecoveryAction::StopAndSuspend
    );
    assert_eq!(
        decide_post_materialization_recovery(
            RecoverySourcePhase::Reserved,
            RecoveryReplayability::NonReplayableCurrent,
            RecoveryMaterialization::ColdOwnerTakeover,
        ),
        RecoveryAction::StopAndFinalizeRecoveryAbort
    );
}

#[test]
fn exact_terminal_interaction_precedes_generic_restart_and_degraded_tokens_do_not_mix() {
    assert_eq!(
        decide_post_materialization_recovery(
            RecoverySourcePhase::StartedOrTransferred {
                exact_terminal_interaction_unavailable: true,
            },
            RecoveryReplayability::NotApplicable,
            RecoveryMaterialization::ColdOwnerTakeover,
        ),
        RecoveryAction::StopAndFinalizeClientCapabilityUnavailable
    );
    assert_eq!(
        decide_post_materialization_recovery(
            RecoverySourcePhase::FinalizingDegraded {
                cause: RecoveryDegradedCause::MissingFinalization,
            },
            RecoveryReplayability::NotApplicable,
            RecoveryMaterialization::ColdOwnerTakeover,
        ),
        RecoveryAction::ExposeRetryFinalization
    );
    assert_eq!(
        decide_post_materialization_recovery(
            RecoverySourcePhase::FinalizingDegraded {
                cause: RecoveryDegradedCause::TerminalProjectionPending,
            },
            RecoveryReplayability::NotApplicable,
            RecoveryMaterialization::ColdOwnerTakeover,
        ),
        RecoveryAction::ExposeRetryProjection
    );
}

#[derive(Default)]
struct FakeSettlementStore {
    existing: std::collections::BTreeMap<SurfaceSettlementId, Sha256Digest>,
    applied: Vec<SurfaceSettlementId>,
}

impl ExternalSettlementStore for FakeSettlementStore {
    fn probe(&self, id: &SurfaceSettlementId) -> Option<SurfaceSettlementReceipt> {
        self.existing
            .get(id)
            .map(|digest| SurfaceSettlementReceipt {
                settlement_id: id.clone(),
                receipt_digest: digest.clone(),
            })
    }

    fn apply_idempotent(
        &mut self,
        id: &SurfaceSettlementId,
    ) -> Result<SurfaceSettlementReceipt, SettlementError> {
        self.applied.push(id.clone());
        let receipt = SurfaceSettlementReceipt {
            settlement_id: id.clone(),
            receipt_digest: digest(99),
        };
        self.existing
            .insert(id.clone(), receipt.receipt_digest.clone());
        Ok(receipt)
    }
}

fn settlement(seed: u8) -> SurfaceSettlementId {
    SurfaceSettlementId::try_from_bytes(uuid(seed)).unwrap()
}

#[test]
fn recovery_settles_only_missing_cross_store_finalize_intent_members() {
    let first = settlement(30);
    let second = settlement(31);
    let mut store = FakeSettlementStore::default();
    store.existing.insert(first.clone(), digest(30));
    let intent = DurableFinalizeIntent::new(
        SurfaceFinalizeIntentId::try_from_bytes(uuid(32)).unwrap(),
        vec![first.clone(), second.clone()],
    )
    .unwrap();
    let receipts = reconcile_finalize_intent(&intent, &mut store).unwrap();
    assert_eq!(store.applied, [second]);
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].settlement_id, first);
}

fn shutdown_plan(seed: u8) -> ShutdownBarrierPlan {
    let thread_id = SurfaceThreadId::try_from_bytes([seed; 16]).unwrap();
    let host = HostIncarnation::try_from_bytes(uuid(seed.wrapping_add(1))).unwrap();
    ShutdownBarrierPlan::CloseThread {
        request_id: SurfaceRequestId::try_from_bytes(uuid(seed.wrapping_add(2))).unwrap(),
        host_incarnation: host.clone(),
        thread: ShutdownThreadPlan::Recorded {
            thread_id: thread_id.clone(),
            owner_epoch: ThreadOwnerEpoch::new(1),
            operations: Vec::new(),
            session_closed: ThreadCursorAckRequirement {
                thread_id: thread_id.clone(),
                family: SurfaceFactFamily::Session,
                event_id: SurfaceEventId::try_from_bytes(uuid(seed.wrapping_add(3))).unwrap(),
                commit_id: SurfaceCommitId::try_from_bytes(uuid(seed.wrapping_add(4))).unwrap(),
            },
            catalog_closed: HostReceiptAckRequirement {
                host_incarnation: host.clone(),
                identity: HostReceiptRequirementIdentity::SessionCatalog {
                    thread_id: Some(thread_id),
                    revision: SessionCatalogRevision::try_new(1).unwrap(),
                },
                commit_id: SurfaceCommitId::try_from_bytes(uuid(seed.wrapping_add(5))).unwrap(),
                receipt_digest: digest(seed),
            },
        },
        barrier_id: settlement(seed.wrapping_add(6)),
        closing_commit_id: SurfaceCommitId::try_from_bytes(uuid(seed.wrapping_add(7))).unwrap(),
        plan_digest: digest(seed.wrapping_add(8)),
    }
}

fn ephemeral_shutdown_plan(seed: u8) -> ShutdownBarrierPlan {
    let thread_id = SurfaceThreadId::try_from_bytes([seed; 16]).unwrap();
    ShutdownBarrierPlan::CloseThread {
        request_id: SurfaceRequestId::try_from_bytes(uuid(seed.wrapping_add(2))).unwrap(),
        host_incarnation: HostIncarnation::try_from_bytes(uuid(seed.wrapping_add(1))).unwrap(),
        thread: ShutdownThreadPlan::Ephemeral {
            thread_id,
            owner_epoch: ThreadOwnerEpoch::new(1),
            persistence: EphemeralThreadPersistence::EphemeralAttached,
            operations: Vec::new(),
            session_closed: ThreadCursorAckRequirement {
                thread_id: SurfaceThreadId::try_from_bytes([seed; 16]).unwrap(),
                family: SurfaceFactFamily::Session,
                event_id: SurfaceEventId::try_from_bytes(uuid(seed.wrapping_add(3))).unwrap(),
                commit_id: SurfaceCommitId::try_from_bytes(uuid(seed.wrapping_add(4))).unwrap(),
            },
        },
        barrier_id: settlement(seed.wrapping_add(6)),
        closing_commit_id: SurfaceCommitId::try_from_bytes(uuid(seed.wrapping_add(7))).unwrap(),
        plan_digest: digest(seed.wrapping_add(8)),
    }
}

fn recorded_shutdown_thread_fixture(
    seed: u8,
) -> (
    HostIncarnation,
    ShutdownThreadPlan,
    Vec<MutationCommitAck>,
    ClosedThreadReceipt,
) {
    let ShutdownBarrierPlan::CloseThread {
        host_incarnation,
        thread:
            ShutdownThreadPlan::Recorded {
                thread_id,
                owner_epoch,
                mut operations,
                session_closed,
                catalog_closed,
            },
        ..
    } = shutdown_plan(seed)
    else {
        unreachable!();
    };
    let operation_id = SurfaceOperationId::try_from_bytes(uuid(seed.wrapping_add(20))).unwrap();
    let finalize_intent_id =
        SurfaceFinalizeIntentId::try_from_bytes(uuid(seed.wrapping_add(21))).unwrap();
    let terminal_commit_id = SurfaceCommitId::try_from_bytes(uuid(seed.wrapping_add(22))).unwrap();
    let requirement = OperationTerminalAckRequirement {
        thread_id: thread_id.clone(),
        thread_owner_epoch: owner_epoch,
        operation_id: operation_id.clone(),
        terminal_commit_id: terminal_commit_id.clone(),
    };
    operations.push(ShutdownOperationPlan::ExistingTerminal {
        operation_id: operation_id.clone(),
        finalize_intent_id,
        terminal_commit_id: terminal_commit_id.clone(),
        requirement,
    });
    let closed_cursor = SurfaceCursor {
        thread_id: thread_id.clone(),
        next_seq: SequenceNumber::new(3),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(3).unwrap(),
        },
        ..cursor(0)
    };
    let terminal = OperationTerminalAtCursor {
        operation_id: operation_id.clone(),
        terminal: OperationTerminal::Cancelled {
            reason: CancelReason::User,
        },
        cursor: SurfaceCursor {
            next_seq: SequenceNumber::new(2),
            ..closed_cursor.clone()
        },
        commit_class: CommitClass::Recorded {
            thread_owner_epoch: owner_epoch,
            durable_revision: DurableRevision::try_new(2).unwrap(),
            commit_id: terminal_commit_id,
        },
        batch_digest: digest(seed.wrapping_add(23)),
    };
    let catalog_receipt = match &catalog_closed.identity {
        HostReceiptRequirementIdentity::SessionCatalog {
            thread_id,
            revision,
        } => SurfaceSessionCatalogReceipt {
            catalog_revision: *revision,
            thread_id: thread_id.clone(),
            action: SurfaceSessionCatalogAction::Closed,
        },
        _ => unreachable!(),
    };
    let acknowledgements = vec![
        MutationCommitAck::OperationTerminalAck {
            thread_id: thread_id.clone(),
            thread_owner_epoch: owner_epoch,
            operation_id,
            value: terminal.clone(),
        },
        MutationCommitAck::ThreadLocalCursor {
            cursor: closed_cursor.clone(),
            family: session_closed.family,
            event_id: session_closed.event_id.clone(),
            commit_class: CommitClass::Recorded {
                thread_owner_epoch: owner_epoch,
                durable_revision: DurableRevision::try_new(3).unwrap(),
                commit_id: session_closed.commit_id.clone(),
            },
        },
        MutationCommitAck::HostCommitAck {
            host_incarnation: catalog_closed.host_incarnation.clone(),
            identity: HostReceiptIdentityPair::SessionCatalog {
                thread_id: catalog_receipt.thread_id.clone(),
                revision: catalog_receipt.catalog_revision,
                receipt: catalog_receipt.clone(),
            },
            commit_id: catalog_closed.commit_id.clone(),
            receipt_digest: catalog_closed.receipt_digest.clone(),
        },
    ];
    let thread = ShutdownThreadPlan::Recorded {
        thread_id: thread_id.clone(),
        owner_epoch,
        operations,
        session_closed,
        catalog_closed,
    };
    let output = ClosedThreadReceipt::Recorded {
        thread_id,
        operation_terminals: vec![terminal],
        closed_cursor,
        catalog_receipt,
    };
    (host_incarnation, thread, acknowledgements, output)
}

#[test]
fn typed_shutdown_codec_round_trips_recorded_thread_and_host_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let (host, thread, acknowledgements, thread_output) = recorded_shutdown_thread_fixture(90);
    let close_plan = ShutdownBarrierPlan::CloseThread {
        request_id: SurfaceRequestId::try_from_bytes(uuid(111)).unwrap(),
        host_incarnation: host.clone(),
        thread: thread.clone(),
        barrier_id: settlement(112),
        closing_commit_id: SurfaceCommitId::try_from_bytes(uuid(113)).unwrap(),
        plan_digest: digest(114),
    };
    let mut close = ImmutableShutdownLedger::default();
    close.record(close_plan.clone()).unwrap();
    let close_store = JsonlSurfaceControlLedger::new(dir.path().join("recorded-close.jsonl"));
    close_store.persist_shutdown_barrier(&mut close).unwrap();
    for acknowledgement in acknowledgements.clone() {
        close.settle(acknowledgement).unwrap();
    }
    let close_output = RetainedShutdownOutput::CloseThread {
        output: thread_output.clone(),
    };
    close.close(close_output.clone()).unwrap();
    close_store.persist_shutdown_barrier(&mut close).unwrap();
    assert_eq!(
        close_store
            .load_shutdown_barrier(&settlement(112))
            .unwrap()
            .unwrap()
            .retained_output(),
        Some(&close_output)
    );

    let barrier_id = settlement(115);
    let host_commit_id = SurfaceCommitId::try_from_bytes(uuid(116)).unwrap();
    let lifecycle_revision = HostLifecycleRevision::try_new(1).unwrap();
    let host_requirement = HostReceiptAckRequirement {
        host_incarnation: host.clone(),
        identity: HostReceiptRequirementIdentity::HostLifecycle {
            host_incarnation: host.clone(),
            revision: lifecycle_revision,
        },
        commit_id: host_commit_id.clone(),
        receipt_digest: digest(117),
    };
    let host_receipt = SurfaceHostShutdownReceipt {
        host_incarnation: host.clone(),
        lifecycle_revision,
        barrier_id: barrier_id.clone(),
        shutdown_commit_id: host_commit_id.clone(),
        stage: SurfaceHostShutdownStage::Last,
        closed_at: UnixMillis::new(10),
    };
    let host_plan = ShutdownBarrierPlan::ShutdownHost {
        request_id: SurfaceRequestId::try_from_bytes(uuid(118)).unwrap(),
        host_incarnation: host.clone(),
        threads: vec![thread],
        barrier_id: barrier_id.clone(),
        closing_commit_id: SurfaceCommitId::try_from_bytes(uuid(119)).unwrap(),
        final_host_lifecycle: host_requirement.clone(),
        plan_digest: digest(120),
    };
    let mut shutdown = ImmutableShutdownLedger::default();
    shutdown.record(host_plan.clone()).unwrap();
    let host_store = JsonlSurfaceControlLedger::new(dir.path().join("host.jsonl"));
    host_store.persist_shutdown_barrier(&mut shutdown).unwrap();
    for acknowledgement in acknowledgements {
        shutdown.settle(acknowledgement).unwrap();
    }
    shutdown
        .settle(MutationCommitAck::HostCommitAck {
            host_incarnation: host.clone(),
            identity: HostReceiptIdentityPair::HostLifecycle {
                host_incarnation: host.clone(),
                revision: lifecycle_revision,
                receipt: host_receipt.clone(),
            },
            commit_id: host_requirement.commit_id,
            receipt_digest: host_requirement.receipt_digest,
        })
        .unwrap();
    let host_output = RetainedShutdownOutput::ShutdownHost {
        output: ShutdownHostOutput {
            host_incarnation: host,
            host_receipt,
            closed_threads: vec![thread_output],
        },
    };
    shutdown.close(host_output.clone()).unwrap();
    host_store.persist_shutdown_barrier(&mut shutdown).unwrap();
    let reopened = host_store
        .load_shutdown_barrier(&barrier_id)
        .unwrap()
        .unwrap();
    assert_eq!(reopened.plan(), Some(&host_plan));
    assert_eq!(reopened.retained_output(), Some(&host_output));
}

#[test]
fn shutdown_plan_is_immutable_and_existing_terminal_cause_wins() {
    let first = shutdown_plan(40);
    let mut ledger = ImmutableShutdownLedger::default();
    assert_eq!(ledger.record(first.clone()).unwrap(), &first);
    assert!(matches!(
        ledger.record(shutdown_plan(50)),
        Err(ShutdownPlanError::ImmutableConflict)
    ));
    assert_eq!(ledger.plan(), Some(&first));

    let existing = OperationFinalizationCause::Terminalization(TerminalizationCause::UserCancel);
    assert_eq!(
        select_shutdown_cause(Some(existing.clone()), ShutdownRequestCause::HostShutdown),
        ShutdownSelectedCause::ExistingWinning { cause: existing }
    );
    assert_eq!(
        select_shutdown_cause(None, ShutdownRequestCause::ThreadClose),
        ShutdownSelectedCause::Requested {
            cause: ShutdownRequestCause::ThreadClose
        }
    );

    let recorded_plan = shutdown_plan(60);
    let recorded_thread_id = match &recorded_plan {
        ShutdownBarrierPlan::CloseThread { thread, .. } => match thread {
            ShutdownThreadPlan::Recorded { thread_id, .. }
            | ShutdownThreadPlan::Ephemeral { thread_id, .. } => thread_id.clone(),
        },
        ShutdownBarrierPlan::ShutdownHost { .. } => unreachable!(),
    };
    let wrong_output = RetainedShutdownOutput::CloseThread {
        output: ClosedThreadReceipt::Ephemeral {
            thread_id: recorded_thread_id,
            persistence: EphemeralThreadPersistence::EphemeralAttached,
            operation_terminals: Vec::new(),
            closed_cursor: cursor(0),
        },
    };
    let mut recorded = ImmutableShutdownLedger::default();
    recorded.record(recorded_plan).unwrap();
    let recorded_dir = tempfile::tempdir().unwrap();
    JsonlSurfaceControlLedger::new(recorded_dir.path().join("recorded.jsonl"))
        .persist_shutdown_barrier(&mut recorded)
        .unwrap();
    assert!(matches!(
        recorded.close(wrong_output),
        Err(ShutdownPlanError::OutputScopeMismatch)
    ));

    let close_plan = ephemeral_shutdown_plan(60);
    let (thread_id, session_closed) = match &close_plan {
        ShutdownBarrierPlan::CloseThread { thread, .. } => match thread {
            ShutdownThreadPlan::Ephemeral {
                thread_id,
                session_closed,
                ..
            } => (thread_id.clone(), session_closed.clone()),
            ShutdownThreadPlan::Recorded { .. } => unreachable!(),
        },
        ShutdownBarrierPlan::ShutdownHost { .. } => unreachable!(),
    };
    let closed_cursor = SurfaceCursor {
        thread_id: thread_id.clone(),
        next_seq: SequenceNumber::new(1),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(2).unwrap(),
        },
        ..cursor(0)
    };
    let closed_output = RetainedShutdownOutput::CloseThread {
        output: ClosedThreadReceipt::Ephemeral {
            thread_id,
            persistence: EphemeralThreadPersistence::EphemeralAttached,
            operation_terminals: Vec::new(),
            closed_cursor: closed_cursor.clone(),
        },
    };
    let mut closed = ImmutableShutdownLedger::default();
    closed.record(close_plan).unwrap();
    assert!(!closed.signal_authorized());
    let premature_ack = MutationCommitAck::ThreadLocalCursor {
        cursor: closed_cursor.clone(),
        family: session_closed.family,
        event_id: session_closed.event_id.clone(),
        commit_class: CommitClass::Recorded {
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            durable_revision: DurableRevision::try_new(2).unwrap(),
            commit_id: session_closed.commit_id.clone(),
        },
    };
    assert!(matches!(
        closed.settle(premature_ack),
        Err(ShutdownPlanError::MissingDurableBarrier)
    ));
    assert!(matches!(
        closed.close(closed_output.clone()),
        Err(ShutdownPlanError::MissingDurableBarrier)
    ));
    let closed_dir = tempfile::tempdir().unwrap();
    JsonlSurfaceControlLedger::new(closed_dir.path().join("closed.jsonl"))
        .persist_shutdown_barrier(&mut closed)
        .unwrap();
    closed
        .settle(MutationCommitAck::ThreadLocalCursor {
            cursor: closed_cursor,
            family: session_closed.family,
            event_id: session_closed.event_id,
            commit_class: CommitClass::Recorded {
                thread_owner_epoch: ThreadOwnerEpoch::new(1),
                durable_revision: DurableRevision::try_new(2).unwrap(),
                commit_id: session_closed.commit_id,
            },
        })
        .unwrap();
    assert!(closed.signal_authorized());
    assert_eq!(closed.close(closed_output.clone()).unwrap(), closed_output);
    assert_eq!(closed.retained_output(), Some(&closed_output));
    assert_eq!(
        closed.close(closed_output).unwrap(),
        closed.retained_output().unwrap().clone()
    );
}

#[derive(Clone)]
struct FakeClock {
    clock_id: HostMonotonicClockId,
    tick: u64,
    wall_ms: i64,
}

impl InjectedRuntimeClock for FakeClock {
    fn clock_id(&self) -> HostMonotonicClockId {
        self.clock_id.clone()
    }

    fn monotonic_tick(&self) -> u64 {
        self.tick
    }

    fn wall_clock_ms(&self) -> i64 {
        self.wall_ms
    }
}

fn test_owner_lease() -> (tempfile::TempDir, ExclusiveOwnerLease) {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(69)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let owner = ExclusiveOwnerLease::acquire_thread(
        dir.path().join("thread.lock"),
        dir.path().join("thread.epoch"),
        cursor(0).thread_id,
        &clock,
    )
    .unwrap();
    (dir, owner)
}

#[test]
fn thread_and_policy_owner_leases_fail_closed_and_wall_rollback_has_no_authority() {
    let dir = tempfile::tempdir().unwrap();
    let first_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(70)).unwrap(),
        tick: 100,
        wall_ms: 10_000,
    };
    let rollback_clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(71)).unwrap(),
        tick: 1,
        wall_ms: -50_000,
    };

    let thread = ExclusiveOwnerLease::acquire(
        dir.path().join("thread.lock"),
        dir.path().join("thread.epoch"),
        OwnerLeaseKind::Thread,
        &first_clock,
    )
    .unwrap();
    assert!(thread.has_authority(&rollback_clock));
    assert!(matches!(
        ExclusiveOwnerLease::acquire(
            dir.path().join("thread.lock"),
            dir.path().join("thread.epoch"),
            OwnerLeaseKind::Thread,
            &rollback_clock,
        ),
        Err(OwnerLeaseError::AlreadyOwned)
    ));

    let policy = ExclusiveOwnerLease::acquire(
        dir.path().join("policy.lock"),
        dir.path().join("policy.epoch"),
        OwnerLeaseKind::Policy,
        &first_clock,
    )
    .unwrap();
    assert!(matches!(
        ExclusiveOwnerLease::acquire(
            dir.path().join("policy.lock"),
            dir.path().join("policy.epoch"),
            OwnerLeaseKind::Policy,
            &rollback_clock,
        ),
        Err(OwnerLeaseError::AlreadyOwned)
    ));
    assert!(policy.has_authority(&rollback_clock));

    let old_epoch = thread.owner_epoch();
    drop(thread);
    let successor = ExclusiveOwnerLease::acquire(
        dir.path().join("thread.lock"),
        dir.path().join("thread.epoch"),
        OwnerLeaseKind::Thread,
        &rollback_clock,
    )
    .unwrap();
    assert!(successor.owner_epoch() > old_epoch);
}

#[test]
fn process_local_thread_owner_is_exclusive_and_never_creates_files() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(73)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    let thread_id = SurfaceThreadId::try_from_bytes(uuid(74)).unwrap();
    let other_thread_id = SurfaceThreadId::try_from_bytes(uuid(75)).unwrap();

    let owner = ExclusiveOwnerLease::acquire_process_local_thread(thread_id.clone(), &clock)
        .expect("acquire process-local owner");
    assert_eq!(owner.owner_epoch(), 1);
    assert_eq!(owner.kind(), OwnerLeaseKind::Thread);
    assert!(owner.has_authority(&clock));
    assert!(matches!(
        ExclusiveOwnerLease::acquire_process_local_thread(thread_id.clone(), &clock),
        Err(OwnerLeaseError::AlreadyOwned)
    ));
    let other = ExclusiveOwnerLease::acquire_process_local_thread(other_thread_id, &clock)
        .expect("different process-local thread has independent authority");
    assert!(other.has_authority(&clock));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

    drop(owner);
    let reacquired = ExclusiveOwnerLease::acquire_process_local_thread(thread_id, &clock)
        .expect("released process-local authority can be reacquired");
    assert!(reacquired.has_authority(&clock));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn owner_epoch_advance_is_atomic_and_lock_path_is_bound_to_epoch_path() {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock {
        clock_id: HostMonotonicClockId::try_from_bytes(uuid(72)).unwrap(),
        tick: 1,
        wall_ms: 1,
    };
    assert!(matches!(
        ExclusiveOwnerLease::acquire_thread(
            dir.path().join("thread.lock"),
            dir.path().join("different.epoch"),
            cursor(0).thread_id,
            &clock,
        ),
        Err(OwnerLeaseError::IdentityMismatch)
    ));

    let external_epoch = dir.path().join("external-epoch");
    std::fs::write(&external_epoch, "40\n").unwrap();
    let epoch_path = dir.path().join("thread.epoch");
    std::os::unix::fs::symlink(&external_epoch, &epoch_path).unwrap();
    let owner = ExclusiveOwnerLease::acquire_thread(
        dir.path().join("thread.lock"),
        &epoch_path,
        cursor(0).thread_id,
        &clock,
    )
    .unwrap();

    assert_eq!(owner.owner_epoch(), 41);
    assert_eq!(std::fs::read_to_string(&epoch_path).unwrap(), "41\n");
    assert_eq!(std::fs::read_to_string(external_epoch).unwrap(), "40\n");
    assert!(
        !std::fs::symlink_metadata(epoch_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
