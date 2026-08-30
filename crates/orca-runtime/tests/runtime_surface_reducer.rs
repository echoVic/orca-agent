use orca_runtime::surface::*;
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

const MANIFEST: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json"
));

fn uuid_v7_bytes(seed: u32) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..4].copy_from_slice(&seed.to_be_bytes());
    bytes[4..].fill((seed % 251) as u8);
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    bytes
}

fn thread_id() -> SurfaceThreadId {
    SurfaceThreadId::try_from_bytes([1; 16]).unwrap()
}

fn incarnation() -> SurfaceIncarnation {
    SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(2)).unwrap()
}

fn digest(seed: u8) -> Sha256Digest {
    Sha256Digest::new([seed; 32])
}

fn subagent_turn_id() -> SurfaceTurnId {
    SurfaceTurnId::parse("turn_01900000-0000-7000-8000-000000000001").unwrap()
}

fn path() -> CanonicalPath {
    CanonicalPath::try_new(std::env::temp_dir().join("orca-surface-reducer")).unwrap()
}

fn usage() -> UsageTotals {
    UsageTotals {
        input_tokens: 0,
        output_tokens: 0,
        cache_tokens: 0,
        estimated_cost_usd_micros: 0,
    }
}

fn settings() -> SurfaceSettingsSnapshot {
    SurfaceSettingsSnapshot {
        host_revision: SettingsRevision::try_new(1).unwrap(),
        thread_revision: SettingsRevision::try_new(1).unwrap(),
        effective: SurfaceRuntimeSettings {
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
            policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        },
        pending: None,
        frozen_generation_revision: None,
    }
}

fn cursor(next_seq: u64) -> SurfaceCursor {
    SurfaceCursor {
        thread_id: thread_id(),
        incarnation: incarnation(),
        next_seq: SequenceNumber::new(next_seq),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(1).unwrap(),
        },
    }
}

fn snapshot() -> SurfaceSnapshot {
    SurfaceSnapshot {
        cursor: cursor(0),
        thread: SurfaceThreadSnapshot {
            thread_id: thread_id(),
            owner_epoch: ThreadOwnerEpoch::new(1),
            persistence: ThreadPersistence::RecordedCatalogued,
            title: DisplayText::new("reducer test"),
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
            thread_total: usage(),
            active_operation: None,
            goal: None,
            workflow: Vec::new(),
        },
        context: SurfaceContextSnapshot {
            revision: ContextRevision::try_new(1).unwrap(),
            window_id: ContextWindowId::new(),
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
        settings: settings(),
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

fn state() -> SurfaceReducerState {
    SurfaceReducerState::new(snapshot())
}

fn commit_class(seed: u32) -> CommitClass {
    CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        durable_revision: DurableRevision::try_new(1).unwrap(),
        commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
    }
}

fn batch(
    state: &SurfaceReducerState,
    seed: u32,
    events: Vec<(SurfaceScope, SurfaceEvent)>,
) -> SurfaceCommitBatch {
    let class = commit_class(seed);
    let before = state.snapshot().cursor.clone();
    let after = SurfaceCursor {
        next_seq: SequenceNumber::new(before.next_seq.get() + events.len() as u64),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(1).unwrap(),
        },
        ..before.clone()
    };
    let envelopes = events
        .into_iter()
        .enumerate()
        .map(|(ordinal, (scope, event))| SurfaceEventEnvelope {
            ordinal: ordinal as u32,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(
                seed.wrapping_mul(2_000).wrapping_add(ordinal as u32 + 1),
            ))
            .unwrap(),
            commit_class: class.clone(),
            scope,
            event,
        })
        .collect::<Vec<_>>();
    let mut batch = SurfaceCommitBatch {
        cursor_before: before,
        cursor_after: after,
        commit_class: class,
        event_count: envelopes.len() as u32,
        batch_digest: digest(0),
        events: NonEmptyVec::try_new(envelopes).unwrap(),
    };
    batch.batch_digest = canonical_batch_digest(&batch);
    batch
}

fn ephemeral_batch(
    state: &SurfaceReducerState,
    seed: u32,
    events: Vec<(SurfaceScope, SurfaceEvent)>,
) -> SurfaceCommitBatch {
    let class = CommitClass::Ephemeral {
        incarnation: incarnation(),
        live_revision: LiveRevision::try_new(seed as u64 + 1).unwrap(),
        commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
    };
    let before = state.snapshot().cursor.clone();
    let after = SurfaceCursor {
        next_seq: SequenceNumber::new(before.next_seq.get() + events.len() as u64),
        source_revision: CursorSourceRevision::Ephemeral {
            live_revision: LiveRevision::try_new(seed as u64 + 1).unwrap(),
        },
        ..before.clone()
    };
    let envelopes = events
        .into_iter()
        .enumerate()
        .map(|(ordinal, (scope, event))| SurfaceEventEnvelope {
            ordinal: ordinal as u32,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(
                seed.wrapping_mul(2_000).wrapping_add(ordinal as u32 + 1),
            ))
            .unwrap(),
            commit_class: class.clone(),
            scope,
            event,
        })
        .collect::<Vec<_>>();
    let mut batch = SurfaceCommitBatch {
        cursor_before: before,
        cursor_after: after,
        commit_class: class,
        event_count: envelopes.len() as u32,
        batch_digest: digest(0),
        events: NonEmptyVec::try_new(envelopes).unwrap(),
    };
    batch.batch_digest = canonical_batch_digest(&batch);
    batch
}

fn plan_event(revision: u64, explanation: impl Into<String>) -> SurfaceEvent {
    SurfaceEvent::Plan(SurfacePlanSnapshot {
        revision: PlanRevision::try_new(revision).unwrap(),
        explanation: Some(DisplayText::new(explanation)),
        items: Vec::new(),
        causative_generation: None,
    })
}

fn applied(result: SurfaceReduceResult) -> SurfaceReducerState {
    match result {
        SurfaceReduceResult::Applied { state } => state,
        SurfaceReduceResult::AlreadyApplied { .. } => {
            panic!("expected Applied, got AlreadyApplied")
        }
        SurfaceReduceResult::Rejected { error } => panic!(
            "expected Applied, got {:?}: {}",
            error.code,
            error.message.as_str()
        ),
    }
}

fn rejected(result: SurfaceReduceResult, code: SurfaceReducerErrorCode) -> SurfaceReducerError {
    match result {
        SurfaceReduceResult::Rejected { error } => {
            assert_eq!(error.code, code);
            error
        }
        _ => panic!("expected Rejected"),
    }
}

fn manifest_pairs(key: &str) -> HashSet<(String, String)> {
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    manifest[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let row = row.as_array().unwrap();
            (
                row[0].as_str().unwrap().to_owned(),
                row[1].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn reducer_api_is_pure_and_cursor_continuous() {
    let initial = state();
    let original = initial.clone();
    let first = batch(
        &initial,
        10,
        vec![(SurfaceScope::Thread, plan_event(2, "first"))],
    );
    let after_first = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &first));
    assert!(initial == original);
    assert_eq!(after_first.snapshot().cursor, first.cursor_after);

    let second = batch(
        &after_first,
        11,
        vec![(SurfaceScope::Thread, plan_event(3, "second"))],
    );
    assert_eq!(second.cursor_before, first.cursor_after);
    let after_second = applied(reduce_batch(SurfaceReduceMode::Live, &after_first, &second));
    assert_eq!(after_second.snapshot().cursor.next_seq.get(), 2);
    assert_eq!(after_second.snapshot().plan.revision.get(), 3);
}

#[test]
fn preflight_enforces_exact_event_and_canonical_byte_limits() {
    let state = state();
    let events = (0..SURFACE_COMMIT_BATCH_EVENT_LIMIT)
        .map(|index| {
            (
                SurfaceScope::Thread,
                SurfaceEvent::Session(SessionPatch::RuntimeFault {
                    class: FailureClass::RuntimeInvariant,
                    message: DisplayText::new(format!("fault-{index}")),
                    causative_generation: None,
                }),
            )
        })
        .collect();
    let at_event_limit = batch(&state, 20, events);
    assert!(matches!(
        preflight_batch(&at_event_limit),
        SurfaceCommitBatchPreflightResult::Ready {
            event_count: 1_024,
            ..
        }
    ));

    let mut over_events = at_event_limit.clone();
    let mut envelopes = over_events.events.as_slice().to_vec();
    let mut extra = envelopes[0].clone();
    extra.ordinal = 1_024;
    extra.event_id = SurfaceEventId::try_from_bytes(uuid_v7_bytes(99_999)).unwrap();
    envelopes.push(extra);
    over_events.events = NonEmptyVec::try_new(envelopes).unwrap();
    over_events.event_count = 1_025;
    over_events.cursor_after.next_seq = SequenceNumber::new(1_025);
    over_events.batch_digest = canonical_batch_digest(&over_events);
    assert!(matches!(
        preflight_batch(&over_events),
        SurfaceCommitBatchPreflightResult::Rejected {
            code: SurfaceCommitBatchPreflightErrorCode::CommitBatchTooLarge,
            observed_event_count: 1_025,
            event_limit: 1_024,
            byte_limit: 8_388_608,
            ..
        }
    ));

    let empty_payload = batch(&state, 21, vec![(SurfaceScope::Thread, plan_event(2, ""))]);
    let base = canonical_batch_encoded_bytes(&empty_payload);
    assert!(base < SURFACE_COMMIT_BATCH_BYTE_LIMIT);
    let below_payload_len = (SURFACE_COMMIT_BATCH_BYTE_LIMIT - base - 256) as usize;
    let at_byte_limit = batch(
        &state,
        22,
        vec![(
            SurfaceScope::Thread,
            plan_event(2, "x".repeat(below_payload_len)),
        )],
    );
    let below_bytes = canonical_batch_encoded_bytes(&at_byte_limit);
    assert!(below_bytes <= SURFACE_COMMIT_BATCH_BYTE_LIMIT);
    assert!(matches!(
        preflight_batch(&at_byte_limit),
        SurfaceCommitBatchPreflightResult::Ready {
            canonical_encoded_bytes,
            ..
        } if canonical_encoded_bytes == below_bytes
    ));

    let over_byte_limit = batch(
        &state,
        23,
        vec![(
            SurfaceScope::Thread,
            plan_event(2, "x".repeat(below_payload_len + 512)),
        )],
    );
    let over_bytes = canonical_batch_encoded_bytes(&over_byte_limit);
    assert!(over_bytes > SURFACE_COMMIT_BATCH_BYTE_LIMIT);
    assert!(matches!(
        preflight_batch(&over_byte_limit),
        SurfaceCommitBatchPreflightResult::Rejected {
            code: SurfaceCommitBatchPreflightErrorCode::CommitBatchTooLarge,
            observed_canonical_encoded_bytes,
            ..
        } if observed_canonical_encoded_bytes == over_bytes
    ));
}

#[test]
fn structural_failures_have_exact_codes_locations_and_no_mutation() {
    let state = state();
    let original = state.clone();
    let valid = batch(
        &state,
        30,
        vec![(SurfaceScope::Thread, plan_event(2, "valid"))],
    );

    let mut wrong_cursor = valid.clone();
    wrong_cursor.cursor_before.next_seq = SequenceNumber::new(9);
    let error = rejected(
        reduce_batch(SurfaceReduceMode::Live, &state, &wrong_cursor),
        SurfaceReducerErrorCode::CursorMismatch,
    );
    assert!(matches!(
        error.location,
        SurfaceReducerErrorLocation::Batch { .. }
    ));

    let mut wrong_count = valid.clone();
    wrong_count.event_count = 2;
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &state, &wrong_count),
        SurfaceReducerErrorCode::InvalidOrdering,
    );

    let mut wrong_ordinal = valid.clone();
    let mut events = wrong_ordinal.events.as_slice().to_vec();
    events[0].ordinal = 1;
    wrong_ordinal.events = NonEmptyVec::try_new(events).unwrap();
    let error = rejected(
        reduce_batch(SurfaceReduceMode::Live, &state, &wrong_ordinal),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
    assert!(matches!(
        error.location,
        SurfaceReducerErrorLocation::Event { ordinal: 1, .. }
    ));

    let mut wrong_scope = valid.clone();
    let mut events = wrong_scope.events.as_slice().to_vec();
    events[0].scope = SurfaceScope::Operation {
        operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(31)).unwrap(),
    };
    wrong_scope.events = NonEmptyVec::try_new(events).unwrap();
    wrong_scope.batch_digest = canonical_batch_digest(&wrong_scope);
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &state, &wrong_scope),
        SurfaceReducerErrorCode::ScopeMismatch,
    );

    let mut wrong_class = valid.clone();
    let mut events = wrong_class.events.as_slice().to_vec();
    events[0].commit_class = commit_class(32);
    wrong_class.events = NonEmptyVec::try_new(events).unwrap();
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &state, &wrong_class),
        SurfaceReducerErrorCode::CommitClassMismatch,
    );

    let mut wrong_revision = valid.clone();
    wrong_revision.cursor_after.source_revision = CursorSourceRevision::Recorded {
        durable_revision: DurableRevision::try_new(2).unwrap(),
    };
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &state, &wrong_revision),
        SurfaceReducerErrorCode::StaleRevision,
    );

    let mut wrong_digest = valid.clone();
    wrong_digest.batch_digest = digest(99);
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &state, &wrong_digest),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
    assert!(state == original);
}

#[test]
fn complete_batch_failure_is_atomic() {
    let state = state();
    let original = state.clone();
    let task = task(SurfaceTaskStatus::Running, 1);
    let events = vec![
        (
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::Upserted {
                expected_revision: None,
                task: task.clone(),
            }),
        ),
        (
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::StatusChanged {
                task_id: task.task_id,
                expected_revision: TaskRevision::try_new(9).unwrap(),
                next_revision: TaskRevision::try_new(10).unwrap(),
                status: SurfaceTaskStatus::Completed,
                completed_at: Some(UnixMillis::new(3)),
                result: None,
                error: None,
            }),
        ),
    ];
    let invalid = batch(&state, 40, events);
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &state, &invalid),
        SurfaceReducerErrorCode::StaleRevision,
    );
    assert!(state == original);
    assert!(state.snapshot().tasks.is_empty());
}

#[test]
fn frozen_event_families_reject_wrong_scope_classes() {
    let fence = operation_fence();
    let wrong_scoped = vec![
        (
            SurfaceScope::Thread,
            SurfaceEvent::Item(ItemPatch::Added {
                item: SurfaceItem::SystemMessage {
                    id: SurfaceItemId::new(),
                    content: DisplayText::new("wrong scope"),
                    pinned: false,
                    origin: SurfaceItemOrigin::RuntimeContext,
                },
            }),
        ),
        (
            SurfaceScope::Thread,
            SurfaceEvent::Assistant(AssistantPatch::StreamOpened {
                stream: SurfaceAssistantStream {
                    stream_id: SurfaceStreamId::try_from_bytes(uuid_v7_bytes(41)).unwrap(),
                    fence: fence.clone(),
                    turn_id: SurfaceTurnId::new(),
                    item_id: SurfaceItemId::new(),
                    channel: AssistantChannel::Message,
                    next_offset: ByteOffset::new(0),
                    text: DisplayText::new(""),
                    state: SurfaceAssistantStreamState::Open,
                },
            }),
        ),
        (
            SurfaceScope::Thread,
            SurfaceEvent::Tool(ToolPatch::Requested {
                request: SurfaceToolRequest {
                    tool_call_id: SurfaceToolCallId::try_new("wrong-scope-tool").unwrap(),
                    source_response_id: None,
                    turn_id: SurfaceTurnId::new(),
                    name: NonEmptyText::try_new("bash").unwrap(),
                    action: SurfaceToolAction::Shell,
                    target: None,
                    raw_arguments: DisplayText::new("{}"),
                    arguments_digest: digest(42),
                },
            }),
        ),
        (
            SurfaceScope::Thread,
            SurfaceEvent::Interaction(InteractionPatch::Requested {
                interaction: SurfaceInteractionView {
                    interaction_id: SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(43))
                        .unwrap(),
                    revision: InteractionRevision::try_new(1).unwrap(),
                    fence: fence.clone(),
                    kind: SurfaceInteractionKind::UserInput,
                    request: SurfaceInteractionRequest::UserInput {
                        question: NonEmptyText::try_new("wrong scope?").unwrap(),
                        suggestions: Vec::new(),
                    },
                    route: SurfaceInteractionRoute::Unassigned {
                        epoch: ResponseRouteEpoch::try_new(1).unwrap(),
                    },
                    lifecycle: SurfaceInteractionLifecycle::Requested,
                    recovery_disposition: InteractionUnavailableDisposition::FailOperation,
                },
            }),
        ),
        (
            SurfaceScope::Generation {
                fence: fence.clone(),
            },
            SurfaceEvent::Workflow(WorkflowPatch::Started {
                workflow: workflow(SurfaceWorkflowStatus::Running),
            }),
        ),
        (
            SurfaceScope::Thread,
            SurfaceEvent::Subagent(SubagentPatch::Started {
                expected_revision: ExpectedAbsentSubagentRevision,
                subagent: RunningSurfaceSubagent::try_new(subagent(
                    SurfaceSubagentStatus::Running,
                    1,
                ))
                .unwrap(),
            }),
        ),
    ];

    for (index, (scope, event)) in wrong_scoped.into_iter().enumerate() {
        let initial = state();
        let invalid = batch(&initial, 41_000 + index as u32, vec![(scope, event)]);
        rejected(
            reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            SurfaceReducerErrorCode::ScopeMismatch,
        );
    }
}

#[test]
fn missing_tool_argument_progress_is_rejected() {
    let initial = state();
    let invalid = batch(
        &initial,
        41_100,
        vec![(
            SurfaceScope::Generation {
                fence: operation_fence(),
            },
            SurfaceEvent::Tool(ToolPatch::ArgumentsProgress {
                tool_call_id: SurfaceToolCallId::try_new("missing-tool").unwrap(),
                arguments_bytes: ByteCount::new(1),
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::MissingIdentity,
    );
}

#[test]
fn pre_request_tool_argument_progress_is_consumed_without_snapshot_state() {
    let (initial, generation) = active_generation_state();
    let tool_call_id = SurfaceToolCallId::try_new("pre-request-tool").unwrap();
    let progress = ephemeral_batch(
        &initial,
        41_101,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::ArgumentsProgress {
                tool_call_id: tool_call_id.clone(),
                arguments_bytes: ByteCount::new(5),
            }),
        )],
    );
    let after_progress = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &progress));
    assert!(after_progress.snapshot().tools.is_empty());

    let requested = batch(
        &after_progress,
        41_102,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::Requested {
                request: SurfaceToolRequest {
                    tool_call_id,
                    source_response_id: None,
                    turn_id: generation.logical_turn_id,
                    name: NonEmptyText::try_new("bash").unwrap(),
                    action: SurfaceToolAction::Shell,
                    target: None,
                    raw_arguments: DisplayText::new("{\"x\":1}"),
                    arguments_digest: digest(42),
                },
            }),
        )],
    );
    let after_request = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &after_progress,
        &requested,
    ));

    assert_eq!(after_request.snapshot().tools.len(), 1);
    assert_eq!(
        after_request.snapshot().tools[0].arguments_bytes,
        ByteCount::new(7)
    );
}

fn active_generation_state() -> (SurfaceReducerState, GenerationRecord) {
    let mut operation = operation_record();
    let mut active_generation = generation(&operation);
    active_generation.phase = GenerationPhase::Started;
    active_generation.started_witness = Some(GenerationStartedWitness {
        started_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(41_201)).unwrap(),
        settings_revision: SettingsRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        durable_replayability_digest: digest(43),
        capability_fingerprint: active_generation.capability_fingerprint.clone(),
    });
    operation.phase = OperationPhase::Admitted;
    operation.initial_logical_turn_id = Some(active_generation.logical_turn_id.clone());
    operation.generations.push(active_generation.clone());
    let mut initial_snapshot = snapshot();
    initial_snapshot.foreground_operation = Some(operation);
    (
        SurfaceReducerState::new(initial_snapshot),
        active_generation,
    )
}

#[test]
fn provider_response_rejects_an_extra_unpaired_tool_request() {
    let (initial, generation) = active_generation_state();
    let response_id = UuidV7::try_from_bytes(uuid_v7_bytes(41_202)).unwrap();
    let paired = SurfaceToolRequest {
        tool_call_id: SurfaceToolCallId::try_new("paired").unwrap(),
        source_response_id: Some(response_id.clone()),
        turn_id: generation.logical_turn_id.clone(),
        name: NonEmptyText::try_new("bash").unwrap(),
        action: SurfaceToolAction::Shell,
        target: None,
        raw_arguments: DisplayText::new("{}"),
        arguments_digest: digest(44),
    };
    let extra = SurfaceToolRequest {
        tool_call_id: SurfaceToolCallId::try_new("extra").unwrap(),
        source_response_id: Some(response_id.clone()),
        turn_id: generation.logical_turn_id.clone(),
        name: NonEmptyText::try_new("write_file").unwrap(),
        action: SurfaceToolAction::Write,
        target: Some(DisplayText::new("/tmp/extra")),
        raw_arguments: DisplayText::new("{\"path\":\"/tmp/extra\"}"),
        arguments_digest: digest(45),
    };
    let invalid = batch(
        &initial,
        41_203,
        vec![
            (
                generation_scope(&generation),
                SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted {
                    response: SurfaceCompletedModelResponse {
                        response_id,
                        turn_id: generation.logical_turn_id.clone(),
                        message_item: None,
                        reasoning_item: None,
                        plan_item: None,
                        tool_calls: vec![SurfaceRawToolCall {
                            id: paired.tool_call_id.clone(),
                            name: paired.name.clone(),
                            raw_arguments: paired.raw_arguments.clone(),
                            arguments_digest: paired.arguments_digest.clone(),
                        }],
                    },
                }),
            ),
            (
                generation_scope(&generation),
                SurfaceEvent::Tool(ToolPatch::Requested { request: paired }),
            ),
            (
                generation_scope(&generation),
                SurfaceEvent::Tool(ToolPatch::Requested { request: extra }),
            ),
        ],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
}

fn state_with_capability_call(
    kind: SurfaceCapabilityCallKind,
    call_state: SurfaceCapabilityCallState,
) -> (SurfaceReducerState, GenerationRecord, SurfaceCapabilityCall) {
    let (initial, generation) = active_generation_state();
    let tool_call_id = SurfaceToolCallId::try_new("capability-tool").unwrap();
    let call = SurfaceCapabilityCall {
        call_id: SurfaceCapabilityCallId::try_from_bytes(uuid_v7_bytes(41_210)).unwrap(),
        acp_session_id: NonEmptyText::try_new("acp-session").unwrap(),
        fence: generation.fence.clone(),
        capability_revision: CapabilityRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        kind,
        arguments_digest: digest(47),
        owning_tool_call_id: tool_call_id.clone(),
        state: call_state,
    };
    let mut initial_snapshot = initial.snapshot().clone();
    initial_snapshot.tools.push(SurfaceToolView {
        request: SurfaceToolRequest {
            tool_call_id,
            source_response_id: None,
            turn_id: generation.logical_turn_id.clone(),
            name: NonEmptyText::try_new("bash").unwrap(),
            action: SurfaceToolAction::Shell,
            target: None,
            raw_arguments: DisplayText::new("{}"),
            arguments_digest: digest(48),
        },
        state: SurfaceToolViewState::Running,
        invocation_started: None,
        arguments_bytes: ByteCount::new(2),
        output_bytes: ByteCount::new(0),
        streamed_output: DisplayText::new(""),
        streamed_output_truncated: false,
        result: None,
        capability_calls: vec![call.clone()],
        terminal_leases: Vec::new(),
    });
    (SurfaceReducerState::new(initial_snapshot), generation, call)
}

#[test]
fn capability_call_cannot_skip_the_side_effect_delivery_barrier() {
    let (initial, generation, mut call) = state_with_capability_call(
        SurfaceCapabilityCallKind::WriteTextFile,
        SurfaceCapabilityCallState::Prepared,
    );
    call.state = SurfaceCapabilityCallState::WrittenAwaitingResponse;
    let invalid = batch(
        &initial,
        41_211,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged { call }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn capability_call_success_result_must_match_the_method() {
    let (initial, generation, mut call) = state_with_capability_call(
        SurfaceCapabilityCallKind::ReadTextFile,
        SurfaceCapabilityCallState::WrittenAwaitingResponse,
    );
    call.state = SurfaceCapabilityCallState::Completed {
        result: CapabilityCallResult::WriteTextFileAcknowledged,
        response_digest: digest(49),
    };
    let invalid = batch(
        &initial,
        41_212,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged { call }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

fn capability_method_name(kind: SurfaceCapabilityCallKind) -> &'static str {
    match kind {
        SurfaceCapabilityCallKind::ReadTextFile => "ReadTextFile",
        SurfaceCapabilityCallKind::WriteTextFile => "WriteTextFile",
        SurfaceCapabilityCallKind::TerminalCreate => "TerminalCreate",
        SurfaceCapabilityCallKind::TerminalOutput => "TerminalOutput",
        SurfaceCapabilityCallKind::TerminalWaitForExit => "TerminalWaitForExit",
        SurfaceCapabilityCallKind::TerminalKill => "TerminalKill",
        SurfaceCapabilityCallKind::TerminalRelease => "TerminalRelease",
    }
}

fn capability_success_result(kind: SurfaceCapabilityCallKind, seed: u8) -> CapabilityCallResult {
    match kind {
        SurfaceCapabilityCallKind::ReadTextFile => CapabilityCallResult::ReadTextFile {
            content: AcpCapabilityText::try_new("content").unwrap(),
            content_digest: digest(seed),
        },
        SurfaceCapabilityCallKind::WriteTextFile => CapabilityCallResult::WriteTextFileAcknowledged,
        SurfaceCapabilityCallKind::TerminalCreate => CapabilityCallResult::TerminalCreated {
            terminal_id: SurfaceRemoteTerminalId::try_new(format!("terminal-{seed}")).unwrap(),
        },
        SurfaceCapabilityCallKind::TerminalOutput => CapabilityCallResult::TerminalOutputObserved {
            output: AcpCapabilityText::try_new("output").unwrap(),
            truncated: false,
            exit_status: None,
        },
        SurfaceCapabilityCallKind::TerminalWaitForExit => {
            CapabilityCallResult::TerminalExitObserved {
                exit_status: SurfaceTerminalExitStatus {
                    exit_code: Some(0),
                    signal: None,
                },
            }
        }
        SurfaceCapabilityCallKind::TerminalKill => CapabilityCallResult::TerminalKillAcknowledged,
        SurfaceCapabilityCallKind::TerminalRelease => {
            CapabilityCallResult::TerminalReleaseAcknowledged
        }
    }
}

#[test]
fn capability_success_matrix_executes_every_manifest_method_and_rejects_complements() {
    let methods = [
        SurfaceCapabilityCallKind::ReadTextFile,
        SurfaceCapabilityCallKind::WriteTextFile,
        SurfaceCapabilityCallKind::TerminalCreate,
        SurfaceCapabilityCallKind::TerminalOutput,
        SurfaceCapabilityCallKind::TerminalWaitForExit,
        SurfaceCapabilityCallKind::TerminalKill,
        SurfaceCapabilityCallKind::TerminalRelease,
    ];
    assert_eq!(
        methods
            .iter()
            .map(|kind| capability_method_name(*kind).to_owned())
            .collect::<Vec<_>>(),
        manifest_string_inventory("acp_capability_call_methods")
    );

    for (index, kind) in methods.into_iter().enumerate() {
        let seed = 41_230 + index as u32 * 10;
        let (initial, generation, mut call) =
            state_with_capability_call(kind, SurfaceCapabilityCallState::WrittenAwaitingResponse);
        let success = capability_success_result(kind, 60 + index as u8);
        call.state = SurfaceCapabilityCallState::Completed {
            result: success.clone(),
            response_digest: digest(70 + index as u8),
        };
        let mut initial_snapshot = initial.snapshot().clone();
        let lease_id = UuidV7::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
        let terminal_id =
            SurfaceRemoteTerminalId::try_new(format!("terminal-{}", 60 + index)).unwrap();
        let lease_event = match &success {
            CapabilityCallResult::TerminalCreated { terminal_id } => {
                Some(SurfaceRemoteTerminalLease {
                    lease_id,
                    owning_tool_call_id: call.owning_tool_call_id.clone(),
                    state: SurfaceRemoteTerminalLeaseState::Live {
                        terminal_id: terminal_id.clone(),
                        owner_fence: generation.fence.clone(),
                    },
                })
            }
            CapabilityCallResult::TerminalKillAcknowledged => {
                initial_snapshot.tools[0]
                    .terminal_leases
                    .push(SurfaceRemoteTerminalLease {
                        lease_id: lease_id.clone(),
                        owning_tool_call_id: call.owning_tool_call_id.clone(),
                        state: SurfaceRemoteTerminalLeaseState::KillPending {
                            terminal_id: terminal_id.clone(),
                            owner_fence: generation.fence.clone(),
                        },
                    });
                Some(SurfaceRemoteTerminalLease {
                    lease_id,
                    owning_tool_call_id: call.owning_tool_call_id.clone(),
                    state: SurfaceRemoteTerminalLeaseState::ReleasePending {
                        terminal_id,
                        owner_fence: generation.fence.clone(),
                    },
                })
            }
            CapabilityCallResult::TerminalReleaseAcknowledged => {
                initial_snapshot.tools[0]
                    .terminal_leases
                    .push(SurfaceRemoteTerminalLease {
                        lease_id: lease_id.clone(),
                        owning_tool_call_id: call.owning_tool_call_id.clone(),
                        state: SurfaceRemoteTerminalLeaseState::ReleasePending {
                            terminal_id,
                            owner_fence: generation.fence.clone(),
                        },
                    });
                Some(SurfaceRemoteTerminalLease {
                    lease_id,
                    owning_tool_call_id: call.owning_tool_call_id.clone(),
                    state: SurfaceRemoteTerminalLeaseState::Released,
                })
            }
            _ => None,
        };
        let initial = SurfaceReducerState::new(initial_snapshot);
        let mut events = vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged { call: call.clone() }),
        )];
        if let Some(lease) = lease_event {
            events.push((
                generation_scope(&generation),
                SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged { lease }),
            ));
        }
        let valid = batch(&initial, seed + 2, events);
        applied(reduce_batch(SurfaceReduceMode::Live, &initial, &valid));

        let (wrong_initial, wrong_generation, mut wrong_call) =
            state_with_capability_call(kind, SurfaceCapabilityCallState::WrittenAwaitingResponse);
        wrong_call.state = SurfaceCapabilityCallState::Completed {
            result: if kind == SurfaceCapabilityCallKind::ReadTextFile {
                CapabilityCallResult::WriteTextFileAcknowledged
            } else {
                CapabilityCallResult::ReadTextFile {
                    content: AcpCapabilityText::try_new("wrong").unwrap(),
                    content_digest: digest(99),
                }
            },
            response_digest: digest(98),
        };
        let invalid = batch(
            &wrong_initial,
            seed + 3,
            vec![(
                generation_scope(&wrong_generation),
                SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged { call: wrong_call }),
            )],
        );
        rejected(
            reduce_batch(SurfaceReduceMode::Live, &wrong_initial, &invalid),
            SurfaceReducerErrorCode::IllegalTransition,
        );
    }
}

#[test]
fn tool_completion_requires_all_capability_calls_to_be_terminal() {
    let (initial, generation, call) = state_with_capability_call(
        SurfaceCapabilityCallKind::ReadTextFile,
        SurfaceCapabilityCallState::Prepared,
    );
    let tool = &initial.snapshot().tools[0].request;
    let content = DisplayText::new("done");
    let terminal = SurfaceToolTerminal {
        kind: SurfaceToolResultKind::Success,
        source: ToolTerminalSource::Observed,
        invocation_started: ToolInvocationStarted::Yes,
    };
    let result = SurfaceToolResult {
        tool_call_id: tool.tool_call_id.clone(),
        name: tool.name.clone(),
        terminal: terminal.clone(),
        output: Some(content.clone()),
        error: None,
        exit_code: None,
        truncated: false,
        file_change: None,
    };
    let invalid = batch(
        &initial,
        41_221,
        vec![
            (
                generation_scope(&generation),
                SurfaceEvent::Tool(ToolPatch::Completed {
                    result: result.clone(),
                }),
            ),
            (
                generation_scope(&generation),
                SurfaceEvent::Item(ItemPatch::Added {
                    item: SurfaceItem::ToolResultMessage {
                        id: SurfaceItemId::new(),
                        turn_id: tool.turn_id.clone(),
                        tool_call_id: tool.tool_call_id.clone(),
                        content,
                        terminal,
                        pinned: false,
                    },
                }),
            ),
        ],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
    assert_eq!(
        initial.snapshot().tools[0].capability_calls[0].call_id,
        call.call_id
    );
}

fn state_with_terminal_lease(
    lease_state: SurfaceRemoteTerminalLeaseState,
) -> (
    SurfaceReducerState,
    GenerationRecord,
    SurfaceRemoteTerminalLease,
) {
    let (initial, generation, _) = state_with_capability_call(
        SurfaceCapabilityCallKind::TerminalRelease,
        SurfaceCapabilityCallState::Prepared,
    );
    let lease_state = match lease_state {
        SurfaceRemoteTerminalLeaseState::Live { terminal_id, .. } => {
            SurfaceRemoteTerminalLeaseState::Live {
                terminal_id,
                owner_fence: generation.fence.clone(),
            }
        }
        SurfaceRemoteTerminalLeaseState::KillPending { terminal_id, .. } => {
            SurfaceRemoteTerminalLeaseState::KillPending {
                terminal_id,
                owner_fence: generation.fence.clone(),
            }
        }
        SurfaceRemoteTerminalLeaseState::ReleasePending { terminal_id, .. } => {
            SurfaceRemoteTerminalLeaseState::ReleasePending {
                terminal_id,
                owner_fence: generation.fence.clone(),
            }
        }
        SurfaceRemoteTerminalLeaseState::CleanupAmbiguous { terminal_id, .. } => {
            SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                terminal_id,
                owner_fence: generation.fence.clone(),
            }
        }
        state => state,
    };
    let lease = SurfaceRemoteTerminalLease {
        lease_id: UuidV7::try_from_bytes(uuid_v7_bytes(41_213)).unwrap(),
        owning_tool_call_id: initial.snapshot().tools[0].request.tool_call_id.clone(),
        state: lease_state,
    };
    let mut snapshot = initial.snapshot().clone();
    snapshot.tools[0].capability_calls.clear();
    snapshot.tools[0].terminal_leases.push(lease.clone());
    (SurfaceReducerState::new(snapshot), generation, lease)
}

#[test]
fn remote_terminal_lease_cannot_skip_kill_and_release() {
    let (initial, generation, mut lease) =
        state_with_terminal_lease(SurfaceRemoteTerminalLeaseState::Live {
            terminal_id: SurfaceRemoteTerminalId::try_new("terminal-1").unwrap(),
            owner_fence: operation_fence(),
        });
    lease.state = SurfaceRemoteTerminalLeaseState::Released;
    let invalid = batch(
        &initial,
        41_214,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged { lease }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn remote_terminal_release_requires_same_batch_capability_ack() {
    let (initial, generation, mut lease) =
        state_with_terminal_lease(SurfaceRemoteTerminalLeaseState::ReleasePending {
            terminal_id: SurfaceRemoteTerminalId::try_new("terminal-2").unwrap(),
            owner_fence: operation_fence(),
        });
    lease.state = SurfaceRemoteTerminalLeaseState::Released;
    let invalid = batch(
        &initial,
        41_215,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged { lease }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
}

#[test]
fn terminal_create_completion_requires_same_batch_live_lease() {
    let (initial, generation, mut call) = state_with_capability_call(
        SurfaceCapabilityCallKind::TerminalCreate,
        SurfaceCapabilityCallState::WrittenAwaitingResponse,
    );
    call.state = SurfaceCapabilityCallState::Completed {
        result: CapabilityCallResult::TerminalCreated {
            terminal_id: SurfaceRemoteTerminalId::try_new("terminal-3").unwrap(),
        },
        response_digest: digest(50),
    };
    let invalid = batch(
        &initial,
        41_216,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged { call }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
}

#[test]
fn ambiguous_terminal_lease_requires_its_capability_settlement_pair() {
    let (initial, generation, _) = state_with_capability_call(
        SurfaceCapabilityCallKind::TerminalKill,
        SurfaceCapabilityCallState::Prepared,
    );
    let tool_call_id = initial.snapshot().tools[0].request.tool_call_id.clone();
    let invalid = batch(
        &initial,
        41_217,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                lease: SurfaceRemoteTerminalLease {
                    lease_id: UuidV7::try_from_bytes(uuid_v7_bytes(41_218)).unwrap(),
                    owning_tool_call_id: tool_call_id,
                    state: SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                        terminal_id: Some(
                            SurfaceRemoteTerminalId::try_new("terminal-ambiguous").unwrap(),
                        ),
                        owner_fence: generation.fence,
                    },
                },
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
}

#[test]
fn cleanup_ambiguous_lease_requires_a_known_terminal_identity() {
    let (initial, generation, _) = state_with_capability_call(
        SurfaceCapabilityCallKind::TerminalRelease,
        SurfaceCapabilityCallState::Prepared,
    );
    let tool_call_id = initial.snapshot().tools[0].request.tool_call_id.clone();
    let invalid = batch(
        &initial,
        41_219,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Tool(ToolPatch::RemoteTerminalLeaseChanged {
                lease: SurfaceRemoteTerminalLease {
                    lease_id: UuidV7::try_from_bytes(uuid_v7_bytes(41_220)).unwrap(),
                    owning_tool_call_id: tool_call_id,
                    state: SurfaceRemoteTerminalLeaseState::CleanupAmbiguous {
                        terminal_id: None,
                        owner_fence: generation.fence,
                    },
                },
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

fn interaction_deadline(seed: u32) -> InteractionExpiryDeadline {
    InteractionExpiryDeadline {
        issuing_host_incarnation: HostIncarnation::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
        expires_at: MonotonicInstant {
            clock_id: HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap(),
            tick: MonotonicTick::new(seed as u64 + 10),
        },
        observed_expires_at: Some(UnixMillis::new(seed as i64 + 10)),
    }
}

fn state_with_user_input_interaction(
    route: SurfaceInteractionRoute,
    recovery_disposition: InteractionUnavailableDisposition,
) -> (
    SurfaceReducerState,
    GenerationRecord,
    SurfaceInteractionView,
) {
    let (initial, generation) = active_generation_state();
    let interaction = SurfaceInteractionView {
        interaction_id: SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(41_220)).unwrap(),
        revision: InteractionRevision::try_new(1).unwrap(),
        fence: generation.fence.clone(),
        kind: SurfaceInteractionKind::UserInput,
        request: SurfaceInteractionRequest::UserInput {
            question: NonEmptyText::try_new("continue?").unwrap(),
            suggestions: vec![DisplayText::new("yes")],
        },
        route,
        lifecycle: SurfaceInteractionLifecycle::Requested,
        recovery_disposition,
    };
    let mut snapshot = initial.snapshot().clone();
    snapshot.interactions.push(interaction.clone());
    (SurfaceReducerState::new(snapshot), generation, interaction)
}

#[test]
fn interaction_resolution_receipt_kind_and_projection_must_match_request() {
    let (initial, generation, interaction) = state_with_user_input_interaction(
        SurfaceInteractionRoute::Unassigned {
            epoch: ResponseRouteEpoch::try_new(1).unwrap(),
        },
        InteractionUnavailableDisposition::FailOperation,
    );
    let invalid = batch(
        &initial,
        41_221,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Interaction(InteractionPatch::Resolved {
                interaction_id: interaction.interaction_id,
                expected_revision: InteractionRevision::try_new(1).unwrap(),
                next_revision: InteractionRevision::try_new(2).unwrap(),
                receipt: SurfaceInteractionResolutionReceipt {
                    response_id: SurfaceResponseId::try_from_bytes(uuid_v7_bytes(41_222)).unwrap(),
                    receipt_id: SurfaceResponseReceiptId::try_from_bytes(uuid_v7_bytes(41_223))
                        .unwrap(),
                    kind: SurfaceInteractionKind::ToolApproval,
                    safe_projection: SurfaceInteractionSafeProjection::ToolApproval {
                        allowed: true,
                    },
                },
                continuation: None,
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn interaction_route_change_must_advance_the_epoch() {
    let (initial, generation, interaction) = state_with_user_input_interaction(
        SurfaceInteractionRoute::Unassigned {
            epoch: ResponseRouteEpoch::try_new(1).unwrap(),
        },
        InteractionUnavailableDisposition::FailOperation,
    );
    let invalid = batch(
        &initial,
        41_224,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Interaction(InteractionPatch::RouteChanged {
                interaction_id: interaction.interaction_id,
                expected_revision: InteractionRevision::try_new(1).unwrap(),
                next_revision: InteractionRevision::try_new(2).unwrap(),
                route: SurfaceInteractionRoute::Unassigned {
                    epoch: ResponseRouteEpoch::try_new(1).unwrap(),
                },
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::StaleRevision,
    );
}

#[test]
fn interaction_expiry_requires_the_persisted_deadline() {
    let persisted_deadline = interaction_deadline(41_225);
    let (initial, generation, interaction) = state_with_user_input_interaction(
        SurfaceInteractionRoute::Unassigned {
            epoch: ResponseRouteEpoch::try_new(1).unwrap(),
        },
        InteractionUnavailableDisposition::AwaitCapableAttachment {
            deadline: persisted_deadline,
        },
    );
    let invalid = batch(
        &initial,
        41_227,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Interaction(InteractionPatch::Expired {
                interaction_id: interaction.interaction_id,
                expected_revision: InteractionRevision::try_new(1).unwrap(),
                next_revision: InteractionRevision::try_new(2).unwrap(),
                deadline: interaction_deadline(41_228),
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn interaction_unavailable_reason_must_match_the_persisted_disposition() {
    let persisted_deadline = interaction_deadline(41_229);
    let (initial, generation, interaction) = state_with_user_input_interaction(
        SurfaceInteractionRoute::Unassigned {
            epoch: ResponseRouteEpoch::try_new(1).unwrap(),
        },
        InteractionUnavailableDisposition::FailOperation,
    );
    let invalid = batch(
        &initial,
        41_230,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Interaction(InteractionPatch::Cancelled {
                interaction_id: interaction.interaction_id,
                expected_revision: InteractionRevision::try_new(1).unwrap(),
                next_revision: InteractionRevision::try_new(2).unwrap(),
                reason: InteractionCancelReason::ExpiryAuthorityUnavailable {
                    deadline: persisted_deadline,
                    failure: InteractionExpiryAuthorityFailure::TickArithmeticOverflow {
                        clock_id: HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(41_231))
                            .unwrap(),
                    },
                },
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn generation_owned_families_reject_stale_exact_fences() {
    let (initial, active_generation) = active_generation_state();
    let stale_fence = SurfaceOperationFence {
        generation_id: SurfaceGenerationId::new(active_generation.fence.generation_id.get() + 1),
        ..active_generation.fence.clone()
    };
    let stale_scope = SurfaceScope::Generation {
        fence: stale_fence.clone(),
    };

    let item = batch(
        &initial,
        41_202,
        vec![(
            stale_scope.clone(),
            SurfaceEvent::Item(ItemPatch::Added {
                item: SurfaceItem::SystemMessage {
                    id: SurfaceItemId::new(),
                    content: DisplayText::new("stale item fence"),
                    pinned: false,
                    origin: SurfaceItemOrigin::RuntimeContext,
                },
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &item),
        SurfaceReducerErrorCode::ScopeMismatch,
    );

    let stream_id = SurfaceStreamId::try_from_bytes(uuid_v7_bytes(41_203)).unwrap();
    let mut stream_snapshot = initial.snapshot().clone();
    stream_snapshot
        .assistant_streams
        .push(SurfaceAssistantStream {
            stream_id: stream_id.clone(),
            fence: active_generation.fence.clone(),
            turn_id: active_generation.logical_turn_id.clone(),
            item_id: SurfaceItemId::new(),
            channel: AssistantChannel::Message,
            next_offset: ByteOffset::new(0),
            text: DisplayText::new(""),
            state: SurfaceAssistantStreamState::Open,
        });
    let stream_state = SurfaceReducerState::new(stream_snapshot);
    let assistant = batch(
        &stream_state,
        41_204,
        vec![(
            stale_scope.clone(),
            SurfaceEvent::Assistant(AssistantPatch::Delta {
                stream_id,
                offset: ByteOffset::new(0),
                text: DisplayText::new("stale"),
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &stream_state, &assistant),
        SurfaceReducerErrorCode::ScopeMismatch,
    );

    let tool = batch(
        &initial,
        41_205,
        vec![(
            stale_scope.clone(),
            SurfaceEvent::Tool(ToolPatch::Requested {
                request: SurfaceToolRequest {
                    tool_call_id: SurfaceToolCallId::try_new("stale-fence-tool").unwrap(),
                    source_response_id: None,
                    turn_id: active_generation.logical_turn_id.clone(),
                    name: NonEmptyText::try_new("bash").unwrap(),
                    action: SurfaceToolAction::Shell,
                    target: None,
                    raw_arguments: DisplayText::new("{}"),
                    arguments_digest: digest(44),
                },
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &tool),
        SurfaceReducerErrorCode::ScopeMismatch,
    );

    let interaction_id = SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(41_206)).unwrap();
    let mut interaction_snapshot = initial.snapshot().clone();
    interaction_snapshot
        .interactions
        .push(SurfaceInteractionView {
            interaction_id: interaction_id.clone(),
            revision: InteractionRevision::try_new(1).unwrap(),
            fence: active_generation.fence,
            kind: SurfaceInteractionKind::UserInput,
            request: SurfaceInteractionRequest::UserInput {
                question: NonEmptyText::try_new("stale interaction fence?").unwrap(),
                suggestions: Vec::new(),
            },
            route: SurfaceInteractionRoute::Unassigned {
                epoch: ResponseRouteEpoch::try_new(1).unwrap(),
            },
            lifecycle: SurfaceInteractionLifecycle::Requested,
            recovery_disposition: InteractionUnavailableDisposition::FailOperation,
        });
    let interaction_state = SurfaceReducerState::new(interaction_snapshot);
    let interaction = batch(
        &interaction_state,
        41_207,
        vec![(
            stale_scope,
            SurfaceEvent::Interaction(InteractionPatch::Cancelled {
                interaction_id,
                expected_revision: InteractionRevision::try_new(1).unwrap(),
                next_revision: InteractionRevision::try_new(2).unwrap(),
                reason: InteractionCancelReason::HostShutdown,
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &interaction_state, &interaction),
        SurfaceReducerErrorCode::ScopeMismatch,
    );
}

#[test]
fn assistant_delta_append_is_linear_and_uses_utf8_byte_offsets() {
    let (initial, active_generation) = active_generation_state();
    let stream_id = SurfaceStreamId::try_from_bytes(uuid_v7_bytes(41_232)).unwrap();
    let mut stream_snapshot = initial.snapshot().clone();
    stream_snapshot
        .assistant_streams
        .push(SurfaceAssistantStream {
            stream_id: stream_id.clone(),
            fence: active_generation.fence.clone(),
            turn_id: active_generation.logical_turn_id.clone(),
            item_id: SurfaceItemId::new(),
            channel: AssistantChannel::Message,
            next_offset: ByteOffset::new(0),
            text: DisplayText::new(""),
            state: SurfaceAssistantStreamState::Open,
        });
    let mut state = SurfaceReducerState::new(stream_snapshot.clone());
    let mut replayed = SurfaceReducerState::new(stream_snapshot);
    let mut offset = 0_u64;
    let mut expected = String::new();
    for (index, delta) in ["hello", " ", "\u{4e16}\u{754c}", " ", "\u{1f680}"]
        .into_iter()
        .chain(std::iter::repeat_n("0123456789", 1_000))
        .enumerate()
    {
        let update = batch(
            &state,
            41_233 + index as u32,
            vec![(
                SurfaceScope::Generation {
                    fence: active_generation.fence.clone(),
                },
                SurfaceEvent::Assistant(AssistantPatch::Delta {
                    stream_id: stream_id.clone(),
                    offset: ByteOffset::new(offset),
                    text: DisplayText::new(delta),
                }),
            )],
        );
        state = applied(reduce_batch(SurfaceReduceMode::Live, &state, &update));
        replayed = applied(reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &replayed,
            &update,
        ));
        expected.push_str(delta);
        offset += delta.len() as u64;
    }

    let stream = &state.snapshot().assistant_streams[0];
    assert_eq!(stream.text.as_str(), expected);
    assert_eq!(stream.next_offset.get(), expected.len() as u64);
    assert_eq!(
        expected.len() - "hello \u{4e16}\u{754c} \u{1f680}".len(),
        10_000
    );
    assert!(replayed.snapshot() == state.snapshot());
}

#[test]
fn capability_call_rejects_fence_from_a_different_generation_than_its_tool() {
    let (initial, owning_generation) = active_generation_state();
    let mut cross_generation = owning_generation.clone();
    cross_generation.fence.generation_id =
        SurfaceGenerationId::new(owning_generation.fence.generation_id.get() + 1);
    cross_generation.logical_turn_id = SurfaceTurnId::new();

    let tool_call_id = SurfaceToolCallId::try_new("generation-bound-tool").unwrap();
    let mut initial_snapshot = initial.snapshot().clone();
    initial_snapshot
        .foreground_operation
        .as_mut()
        .unwrap()
        .generations
        .push(cross_generation.clone());
    initial_snapshot.tools.push(SurfaceToolView {
        request: SurfaceToolRequest {
            tool_call_id: tool_call_id.clone(),
            source_response_id: None,
            turn_id: owning_generation.logical_turn_id,
            name: NonEmptyText::try_new("bash").unwrap(),
            action: SurfaceToolAction::Shell,
            target: None,
            raw_arguments: DisplayText::new("{}"),
            arguments_digest: digest(45),
        },
        state: SurfaceToolViewState::Requested,
        invocation_started: None,
        arguments_bytes: ByteCount::new(2),
        output_bytes: ByteCount::new(0),
        streamed_output: DisplayText::new(""),
        streamed_output_truncated: false,
        result: None,
        capability_calls: Vec::new(),
        terminal_leases: Vec::new(),
    });
    let initial = SurfaceReducerState::new(initial_snapshot);
    let invalid = batch(
        &initial,
        41_208,
        vec![(
            SurfaceScope::Generation {
                fence: cross_generation.fence.clone(),
            },
            SurfaceEvent::Tool(ToolPatch::CapabilityCallChanged {
                call: SurfaceCapabilityCall {
                    call_id: SurfaceCapabilityCallId::try_from_bytes(uuid_v7_bytes(41_209))
                        .unwrap(),
                    acp_session_id: NonEmptyText::try_new("acp-session").unwrap(),
                    fence: cross_generation.fence,
                    capability_revision: CapabilityRevision::try_new(1).unwrap(),
                    policy_epoch: PolicyEpoch::try_new(1).unwrap(),
                    kind: SurfaceCapabilityCallKind::TerminalCreate,
                    arguments_digest: digest(46),
                    owning_tool_call_id: tool_call_id,
                    state: SurfaceCapabilityCallState::Prepared,
                },
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::ScopeMismatch,
    );
}

#[test]
fn item_assistant_tool_and_interaction_families_reduce_closed_state() {
    let mut operation = operation_record();
    let mut active_generation = generation(&operation);
    let item_id = SurfaceItemId::new();
    let presentation = SurfaceInputPresentation::Visible {
        text: DisplayText::new("hello"),
    };
    let correlation_id = SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(13_012)).unwrap();
    active_generation.input = GenerationInputState::Pending {
        input_item_id: item_id.clone(),
        presentation: presentation.clone(),
        correlation_id: correlation_id.clone(),
    };
    active_generation.phase = GenerationPhase::Started;
    active_generation.started_witness = Some(GenerationStartedWitness {
        started_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(45)).unwrap(),
        settings_revision: SettingsRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        durable_replayability_digest: digest(46),
        capability_fingerprint: active_generation.capability_fingerprint.clone(),
    });
    operation.phase = OperationPhase::Admitted;
    operation.initial_logical_turn_id = Some(active_generation.logical_turn_id.clone());
    operation.generations.push(active_generation.clone());
    let mut initial_snapshot = snapshot();
    initial_snapshot.foreground_operation = Some(operation);
    initial_snapshot.items.push(SurfaceItem::UserMessage {
        id: item_id.clone(),
        turn_id: active_generation.logical_turn_id.clone(),
        input: SurfaceUserInputState::Pending {
            presentation,
            correlation_id,
        },
        pinned: false,
        origin: SurfaceItemOrigin::UserInput,
    });
    let initial = SurfaceReducerState::new(initial_snapshot);

    let input_fact = SurfaceResolvedInputFact::Replayable {
        input: SurfaceInput {
            blocks: NonEmptyVec::try_new(vec![SurfaceInputBlock::Text {
                text: DisplayText::new("hello"),
            }])
            .unwrap(),
            canonical_text: DisplayText::new("hello"),
            bindings_digest: digest(47),
        },
        request_digest: digest(48),
    };
    let resolve_item = batch(
        &initial,
        13_001,
        vec![
            (
                SurfaceScope::Generation {
                    fence: active_generation.fence.clone(),
                },
                SurfaceEvent::Item(ItemPatch::InputResolved {
                    item_id: item_id.clone(),
                    fact: input_fact.clone(),
                }),
            ),
            (
                SurfaceScope::Generation {
                    fence: active_generation.fence.clone(),
                },
                SurfaceEvent::Operation(OperationPatch::InputBindingsResolved {
                    fence: active_generation.fence.clone(),
                    input_item_id: item_id.clone(),
                    fact: input_fact.clone(),
                }),
            ),
        ],
    );
    let with_resolved_item = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &initial,
        &resolve_item,
    ));

    let stream_id = SurfaceStreamId::try_from_bytes(uuid_v7_bytes(13_013)).unwrap();
    let assistant_item_id = SurfaceItemId::new();
    let open_stream = batch(
        &with_resolved_item,
        13_002,
        vec![(
            SurfaceScope::Generation {
                fence: active_generation.fence.clone(),
            },
            SurfaceEvent::Assistant(AssistantPatch::StreamOpened {
                stream: SurfaceAssistantStream {
                    stream_id: stream_id.clone(),
                    fence: active_generation.fence.clone(),
                    turn_id: active_generation.logical_turn_id.clone(),
                    item_id: assistant_item_id.clone(),
                    channel: AssistantChannel::Message,
                    next_offset: ByteOffset::new(0),
                    text: DisplayText::new(""),
                    state: SurfaceAssistantStreamState::Open,
                },
            }),
        )],
    );
    let stream_open = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &with_resolved_item,
        &open_stream,
    ));
    let delta = batch(
        &stream_open,
        13_003,
        vec![(
            SurfaceScope::Generation {
                fence: active_generation.fence.clone(),
            },
            SurfaceEvent::Assistant(AssistantPatch::Delta {
                stream_id: stream_id.clone(),
                offset: ByteOffset::new(0),
                text: DisplayText::new("answer"),
            }),
        )],
    );
    let streamed = applied(reduce_batch(SurfaceReduceMode::Live, &stream_open, &delta));
    assert_eq!(
        streamed.snapshot().assistant_streams[0].next_offset.get(),
        6
    );

    let response_id = UuidV7::try_from_bytes(uuid_v7_bytes(13_004)).unwrap();
    let response = batch(
        &streamed,
        13_005,
        vec![(
            SurfaceScope::Generation {
                fence: active_generation.fence.clone(),
            },
            SurfaceEvent::Assistant(AssistantPatch::ResponseCompleted {
                response: SurfaceCompletedModelResponse {
                    response_id: response_id.clone(),
                    turn_id: active_generation.logical_turn_id.clone(),
                    message_item: Some(SurfaceAssistantMessageItem {
                        id: assistant_item_id,
                        turn_id: active_generation.logical_turn_id.clone(),
                        text: DisplayText::new("answer"),
                        pinned: false,
                    }),
                    reasoning_item: None,
                    plan_item: None,
                    tool_calls: Vec::new(),
                },
            }),
        )],
    );
    let responded = applied(reduce_batch(SurfaceReduceMode::Live, &streamed, &response));
    assert_eq!(
        responded.snapshot().assistant_streams[0].state,
        SurfaceAssistantStreamState::Completed
    );

    let tool_call_id = SurfaceToolCallId::try_new("tool-13").unwrap();
    let request = SurfaceToolRequest {
        tool_call_id: tool_call_id.clone(),
        source_response_id: None,
        turn_id: active_generation.logical_turn_id.clone(),
        name: NonEmptyText::try_new("bash").unwrap(),
        action: SurfaceToolAction::Shell,
        target: Some(DisplayText::new("cargo test")),
        raw_arguments: DisplayText::new("{}"),
        arguments_digest: digest(49),
    };
    let requested_tool = batch(
        &responded,
        13_006,
        vec![(
            SurfaceScope::Generation {
                fence: active_generation.fence.clone(),
            },
            SurfaceEvent::Tool(ToolPatch::Requested {
                request: request.clone(),
            }),
        )],
    );
    let tool_requested = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &responded,
        &requested_tool,
    ));
    let output = batch(
        &tool_requested,
        13_007,
        vec![(
            SurfaceScope::Generation {
                fence: active_generation.fence.clone(),
            },
            SurfaceEvent::Tool(ToolPatch::OutputDelta {
                tool_call_id: tool_call_id.clone(),
                offset: ByteOffset::new(0),
                chunk: DisplayText::new("ok"),
            }),
        )],
    );
    let tool_output = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &tool_requested,
        &output,
    ));
    let terminal = SurfaceToolTerminal {
        kind: SurfaceToolResultKind::Success,
        source: ToolTerminalSource::Observed,
        invocation_started: ToolInvocationStarted::Yes,
    };
    let result = SurfaceToolResult {
        tool_call_id: tool_call_id.clone(),
        name: request.name.clone(),
        terminal: terminal.clone(),
        output: Some(DisplayText::new("ok")),
        error: None,
        exit_code: Some(0),
        truncated: false,
        file_change: None,
    };
    let complete_tool = batch(
        &tool_output,
        13_008,
        vec![
            (
                SurfaceScope::Generation {
                    fence: active_generation.fence.clone(),
                },
                SurfaceEvent::Tool(ToolPatch::Completed {
                    result: result.clone(),
                }),
            ),
            (
                SurfaceScope::Generation {
                    fence: active_generation.fence.clone(),
                },
                SurfaceEvent::Item(ItemPatch::Added {
                    item: SurfaceItem::ToolResultMessage {
                        id: SurfaceItemId::new(),
                        turn_id: active_generation.logical_turn_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                        content: DisplayText::new("ok"),
                        terminal,
                        pinned: false,
                    },
                }),
            ),
        ],
    );
    let tool_completed = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &tool_output,
        &complete_tool,
    ));
    assert_eq!(
        tool_completed.snapshot().tools[0].state,
        SurfaceToolViewState::Completed
    );

    let interaction_id = SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(13_009)).unwrap();
    let request_interaction = batch(
        &tool_completed,
        13_010,
        vec![(
            SurfaceScope::Generation {
                fence: active_generation.fence.clone(),
            },
            SurfaceEvent::Interaction(InteractionPatch::Requested {
                interaction: SurfaceInteractionView {
                    interaction_id: interaction_id.clone(),
                    revision: InteractionRevision::try_new(1).unwrap(),
                    fence: active_generation.fence.clone(),
                    kind: SurfaceInteractionKind::UserInput,
                    request: SurfaceInteractionRequest::UserInput {
                        question: NonEmptyText::try_new("continue?").unwrap(),
                        suggestions: Vec::new(),
                    },
                    route: SurfaceInteractionRoute::Unassigned {
                        epoch: ResponseRouteEpoch::try_new(1).unwrap(),
                    },
                    lifecycle: SurfaceInteractionLifecycle::Requested,
                    recovery_disposition: InteractionUnavailableDisposition::FailOperation,
                },
            }),
        )],
    );
    let interaction_requested = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &tool_completed,
        &request_interaction,
    ));
    let cancel_interaction = batch(
        &interaction_requested,
        13_011,
        vec![(
            SurfaceScope::Generation {
                fence: active_generation.fence,
            },
            SurfaceEvent::Interaction(InteractionPatch::Cancelled {
                interaction_id,
                expected_revision: InteractionRevision::try_new(1).unwrap(),
                next_revision: InteractionRevision::try_new(2).unwrap(),
                reason: InteractionCancelReason::HostShutdown,
            }),
        )],
    );
    let interaction_cancelled = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &interaction_requested,
        &cancel_interaction,
    ));
    assert!(matches!(
        interaction_cancelled.snapshot().interactions[0].lifecycle,
        SurfaceInteractionLifecycle::Cancelled { .. }
    ));
}

#[test]
fn usage_context_settings_catalog_and_pinned_families_require_exact_revisions() {
    let initial = state();
    let usage_batch = batch(
        &initial,
        14_000,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Usage(SurfaceUsageSnapshot {
                revision: UsageRevision::try_new(2).unwrap(),
                thread_total: UsageTotals {
                    input_tokens: 3,
                    output_tokens: 5,
                    cache_tokens: 1,
                    estimated_cost_usd_micros: 7,
                },
                active_operation: None,
                goal: None,
                workflow: Vec::new(),
            }),
        )],
    );
    let with_usage = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &initial,
        &usage_batch,
    ));

    let context_batch = batch(
        &with_usage,
        14_001,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Context(SurfaceContextSnapshot {
                revision: ContextRevision::try_new(2).unwrap(),
                window_id: with_usage.snapshot().context.window_id.clone(),
                used_tokens: 8,
                limit_tokens: 128_000,
                compaction: CompactionState::Idle,
                fragments: vec![SurfaceContextFragment {
                    id: NonEmptyText::try_new("runtime").unwrap(),
                    kind: SurfaceContextFragmentKind::Runtime,
                    origin: SurfaceContextFragmentOrigin::System,
                    content: DisplayText::new("typed context"),
                    max_tokens: 32,
                }],
                provider_replay: ProviderReplayHealth::Available {
                    state_digest: digest(90),
                },
            }),
        )],
    );
    let with_context = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &with_usage,
        &context_batch,
    ));

    let pending_settings = SurfaceRuntimeSettings {
        model: NonEmptyText::try_new("deepseek-v4.1").unwrap(),
        ..with_context.snapshot().settings.effective.clone()
    };
    let settings_batch = batch(
        &with_context,
        14_002,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Settings(SettingsPatch::PendingChanged {
                thread_revision: SettingsRevision::try_new(2).unwrap(),
                pending: Some(pending_settings.clone()),
            }),
        )],
    );
    let with_settings = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &with_context,
        &settings_batch,
    ));
    assert_eq!(
        with_settings.snapshot().settings.pending,
        Some(pending_settings)
    );

    let mcp_batch = batch(
        &with_settings,
        14_003,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::McpCatalog(McpCatalogPatch::ServerStatusChanged {
                previous_revision: McpCatalogRevision::try_new(1).unwrap(),
                next_revision: McpCatalogRevision::try_new(2).unwrap(),
                server: NonEmptyText::try_new("filesystem").unwrap(),
                status: SurfaceMcpServerStatus::Ready,
            }),
        )],
    );
    let with_mcp = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &with_settings,
        &mcp_batch,
    ));

    let invalid_entry = SurfacePinnedContextEntry {
        id: SurfaceCatalogEntryId::try_new("pin-1").unwrap(),
        kind: SurfacePinnedContextKind::Memory,
        label: NonEmptyText::try_new("memory").unwrap(),
        content: DisplayText::new("remember"),
        content_digest: digest(91),
        source_revision: PinnedContextSourceRevision::File(PinnedFileRevision::try_new(1).unwrap()),
    };
    let invalid_pin = batch(
        &with_mcp,
        14_004,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::PinnedContext(PinnedContextPatch::Added {
                previous_revision: PinnedContextRevision::try_new(1).unwrap(),
                next_revision: PinnedContextRevision::try_new(2).unwrap(),
                entry: invalid_entry.clone(),
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &with_mcp, &invalid_pin),
        SurfaceReducerErrorCode::IllegalTransition,
    );
    assert!(with_mcp.snapshot().pinned_context.entries.is_empty());

    let valid_entry = SurfacePinnedContextEntry {
        source_revision: PinnedContextSourceRevision::Memory(MemoryRevision::try_new(1).unwrap()),
        ..invalid_entry
    };
    let pin_batch = batch(
        &with_mcp,
        14_005,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::PinnedContext(PinnedContextPatch::Added {
                previous_revision: PinnedContextRevision::try_new(1).unwrap(),
                next_revision: PinnedContextRevision::try_new(2).unwrap(),
                entry: valid_entry,
            }),
        )],
    );
    let pinned = applied(reduce_batch(SurfaceReduceMode::Live, &with_mcp, &pin_batch));
    assert_eq!(pinned.snapshot().usage.revision.get(), 2);
    assert_eq!(pinned.snapshot().context.revision.get(), 2);
    assert_eq!(pinned.snapshot().settings.thread_revision.get(), 2);
    assert_eq!(pinned.snapshot().mcp_catalog.revision.get(), 2);
    assert_eq!(pinned.snapshot().pinned_context.revision.get(), 2);
}

#[test]
fn session_metadata_health_and_close_barriers_are_ordered() {
    let initial = state();
    let metadata = batch(
        &initial,
        15_000,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Session(SessionPatch::MetadataChanged {
                previous_revision: SessionMetadataRevision::try_new(1).unwrap(),
                next_revision: SessionMetadataRevision::try_new(2).unwrap(),
                title: DisplayText::new("renamed"),
                updated_at: UnixMillis::new(2),
            }),
        )],
    );
    let with_metadata = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &metadata));

    let settlement_id = SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(15_001)).unwrap();
    let issue_id = SurfaceHealthIssueId::Mutation(settlement_id.clone());
    let issue = SurfaceHealthIssue::MutationDegraded {
        settlement_id: settlement_id.clone(),
    };
    let add_issue = batch(
        &with_metadata,
        15_002,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Session(SessionPatch::HealthIssueAdded {
                previous_revision: SessionHealthRevision::try_new(1).unwrap(),
                next_revision: SessionHealthRevision::try_new(2).unwrap(),
                id: issue_id.clone(),
                issue,
            }),
        )],
    );
    let degraded = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &with_metadata,
        &add_issue,
    ));
    let clear_issue = batch(
        &degraded,
        15_003,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Session(SessionPatch::HealthIssueCleared {
                previous_revision: SessionHealthRevision::try_new(2).unwrap(),
                next_revision: SessionHealthRevision::try_new(3).unwrap(),
                id: issue_id.clone(),
                proof: SurfaceHealthClearProof {
                    issue_id,
                    resolving_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(15_004))
                        .unwrap(),
                    receipt_digest: digest(92),
                },
            }),
        )],
    );
    let healthy = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &degraded,
        &clear_issue,
    ));

    let barrier_id = SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(15_005)).unwrap();
    let closing_commit_id = SurfaceCommitId::try_from_bytes(uuid_v7_bytes(15_006)).unwrap();
    let plan_digest = digest(93);
    let closing = batch(
        &healthy,
        15_007,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Session(SessionPatch::Closing {
                reason: SurfaceShutdownReason::ThreadClose,
                barrier_id: barrier_id.clone(),
                closing_commit_id: closing_commit_id.clone(),
                plan_digest: plan_digest.clone(),
            }),
        )],
    );
    let closing_state = applied(reduce_batch(SurfaceReduceMode::Live, &healthy, &closing));
    let closed = batch(
        &closing_state,
        15_008,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Session(SessionPatch::Closed {
                reason: SurfaceShutdownReason::ThreadClose,
                barrier_id,
                closing_commit_id,
                plan_digest,
            }),
        )],
    );
    let closed_state = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &closing_state,
        &closed,
    ));
    assert_eq!(
        closed_state.snapshot().thread.title,
        DisplayText::new("renamed")
    );
    assert!(closed_state.snapshot().session_health.issues.is_empty());
    assert!(closed_state.snapshot().session_health.closing);
    assert!(closed_state.snapshot().session_health.closed);
    assert!(closed_state.snapshot().thread.closed);
}

#[test]
fn duplicate_rules_distinguish_live_exact_rematerialization_and_conflicts() {
    let initial = state();
    let batch = batch(
        &initial,
        50,
        vec![
            (SurfaceScope::Thread, plan_event(2, "once")),
            (SurfaceScope::Thread, plan_event(3, "twice")),
        ],
    );
    let applied_state = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &batch));

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &applied_state, &batch),
        SurfaceReducerErrorCode::DuplicateTransition,
    );
    assert!(matches!(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &batch
        ),
        SurfaceReduceResult::AlreadyApplied { cursor, .. } if cursor == batch.cursor_after
    ));

    let mut changed_class = batch.clone();
    let commit_id = match &batch.commit_class {
        CommitClass::Recorded { commit_id, .. } => commit_id.clone(),
        _ => unreachable!(),
    };
    changed_class.commit_class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(2),
        durable_revision: DurableRevision::try_new(1).unwrap(),
        commit_id,
    };
    rejected(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &changed_class,
        ),
        SurfaceReducerErrorCode::CommitClassMismatch,
    );

    let mut changed_digest = batch.clone();
    changed_digest.batch_digest = digest(51);
    rejected(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &changed_digest,
        ),
        SurfaceReducerErrorCode::DuplicateTransition,
    );

    let mut changed_boundary = batch.clone();
    changed_boundary.cursor_before.next_seq = SequenceNumber::new(99);
    rejected(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &changed_boundary,
        ),
        SurfaceReducerErrorCode::DuplicateTransition,
    );

    let mut changed_count = batch.clone();
    changed_count.event_count = 1;
    rejected(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &changed_count,
        ),
        SurfaceReducerErrorCode::DuplicateTransition,
    );

    let mut changed_order = batch.clone();
    let mut events = changed_order.events.as_slice().to_vec();
    events.swap(0, 1);
    for (ordinal, event) in events.iter_mut().enumerate() {
        event.ordinal = ordinal as u32;
    }
    changed_order.events = NonEmptyVec::try_new(events).unwrap();
    changed_order.batch_digest = canonical_batch_digest(&changed_order);
    rejected(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &changed_order,
        ),
        SurfaceReducerErrorCode::DuplicateTransition,
    );

    let mut changed_membership = batch.clone();
    let mut events = changed_membership.events.as_slice().to_vec();
    events[1].event = plan_event(3, "changed membership");
    changed_membership.events = NonEmptyVec::try_new(events).unwrap();
    changed_membership.batch_digest = canonical_batch_digest(&changed_membership);
    rejected(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &changed_membership,
        ),
        SurfaceReducerErrorCode::DuplicateTransition,
    );

    let mut changed_event_id = batch.clone();
    let mut events = changed_event_id.events.as_slice().to_vec();
    events[1].event_id = SurfaceEventId::try_from_bytes(uuid_v7_bytes(50_999)).unwrap();
    changed_event_id.events = NonEmptyVec::try_new(events).unwrap();
    changed_event_id.batch_digest = canonical_batch_digest(&changed_event_id);
    rejected(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &changed_event_id,
        ),
        SurfaceReducerErrorCode::DuplicateTransition,
    );
}

fn task(status: SurfaceTaskStatus, revision: u64) -> SurfaceTask {
    SurfaceTask {
        task_id: SurfaceTaskId::try_new("manifest-task").unwrap(),
        revision: TaskRevision::try_new(revision).unwrap(),
        task_type: SurfaceTaskType::MainSession,
        status,
        backgrounded: false,
        description: DisplayText::new("manifest task"),
        created_at: UnixMillis::new(1),
        started_at: Some(UnixMillis::new(2)),
        completed_at: None,
        parent_operation: None,
        parent_task_id: None,
        background_fence: None,
        workflow_run_id: None,
        subagent_id: None,
        pending_interaction_id: None,
        usage: None,
        result: None,
        error: None,
        retry_count: 0,
        output_truncated: false,
    }
}

#[test]
fn task_parent_must_exist_and_cannot_reference_itself() {
    let initial = state();
    let mut missing_parent = task(SurfaceTaskStatus::Running, 1);
    missing_parent.task_id = SurfaceTaskId::try_new("child-task").unwrap();
    missing_parent.parent_task_id = Some(SurfaceTaskId::try_new("missing-task").unwrap());
    let missing = batch(
        &initial,
        5_901,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::Upserted {
                expected_revision: None,
                task: missing_parent,
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &missing),
        SurfaceReducerErrorCode::MissingIdentity,
    );

    let mut self_parent = task(SurfaceTaskStatus::Running, 1);
    self_parent.task_id = SurfaceTaskId::try_new("self-task").unwrap();
    self_parent.parent_task_id = Some(self_parent.task_id.clone());
    let self_cycle = batch(
        &initial,
        5_902,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::Upserted {
                expected_revision: None,
                task: self_parent,
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &self_cycle),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn task_permission_interaction_epochs_are_atomic() {
    let interaction_id = SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(61_001)).unwrap();
    let mut initial_snapshot = snapshot();
    initial_snapshot
        .tasks
        .push(task(SurfaceTaskStatus::Running, 1));
    let initial = SurfaceReducerState::new(initial_snapshot);

    let request = batch(
        &initial,
        61_002,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::InteractionChanged {
                task_id: SurfaceTaskId::try_new("manifest-task").unwrap(),
                expected_revision: TaskRevision::try_new(1).unwrap(),
                next_revision: TaskRevision::try_new(2).unwrap(),
                status: SurfaceTaskStatus::ApprovalRequired,
                pending_interaction_id: Some(interaction_id.clone()),
            }),
        )],
    );
    let after_request = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &request));
    let requested_task = &after_request.snapshot().tasks[0];
    assert_eq!(requested_task.revision.get(), 2);
    assert_eq!(
        requested_task.pending_interaction_id.as_ref(),
        Some(&interaction_id)
    );
    assert_eq!(requested_task.status, SurfaceTaskStatus::ApprovalRequired);

    let stale_resolution = batch(
        &after_request,
        61_003,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::InteractionChanged {
                task_id: SurfaceTaskId::try_new("manifest-task").unwrap(),
                expected_revision: TaskRevision::try_new(1).unwrap(),
                next_revision: TaskRevision::try_new(2).unwrap(),
                status: SurfaceTaskStatus::Running,
                pending_interaction_id: None,
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &after_request, &stale_resolution),
        SurfaceReducerErrorCode::StaleRevision,
    );
    assert_eq!(after_request.snapshot().tasks[0].revision.get(), 2);
    assert_eq!(
        after_request.snapshot().tasks[0]
            .pending_interaction_id
            .as_ref(),
        Some(&interaction_id)
    );

    // A batch containing a valid request followed by a stale duplicate is
    // rejected as a whole; the caller's state remains at the pre-batch epoch.
    let duplicate_request = batch(
        &initial,
        61_004,
        vec![
            (
                SurfaceScope::Thread,
                SurfaceEvent::Task(TaskPatch::InteractionChanged {
                    task_id: SurfaceTaskId::try_new("manifest-task").unwrap(),
                    expected_revision: TaskRevision::try_new(1).unwrap(),
                    next_revision: TaskRevision::try_new(2).unwrap(),
                    status: SurfaceTaskStatus::ApprovalRequired,
                    pending_interaction_id: Some(interaction_id.clone()),
                }),
            ),
            (
                SurfaceScope::Thread,
                SurfaceEvent::Task(TaskPatch::InteractionChanged {
                    task_id: SurfaceTaskId::try_new("manifest-task").unwrap(),
                    expected_revision: TaskRevision::try_new(1).unwrap(),
                    next_revision: TaskRevision::try_new(2).unwrap(),
                    status: SurfaceTaskStatus::ApprovalRequired,
                    pending_interaction_id: Some(interaction_id),
                }),
            ),
        ],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &duplicate_request),
        SurfaceReducerErrorCode::StaleRevision,
    );
    assert!(initial.snapshot().tasks[0].pending_interaction_id.is_none());
    assert_eq!(initial.snapshot().tasks[0].revision.get(), 1);
}

fn operation_fence() -> SurfaceOperationFence {
    SurfaceOperationFence {
        thread_id: thread_id(),
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(6_000)).unwrap(),
        generation_id: SurfaceGenerationId::new(0),
    }
}

fn subagent_owner() -> SurfaceSubagentOwner {
    SurfaceSubagentOwner::Generation {
        fence: operation_fence(),
    }
}

fn subagent_source(sequence: u64) -> SurfaceSubagentSource {
    SurfaceSubagentSource::new(
        SurfaceTaskAttemptId::try_new("manifest-attempt").unwrap(),
        subagent_turn_id(),
        sequence,
        SurfaceCommitId::try_from_bytes(uuid_v7_bytes(6_100u32.wrapping_add(sequence as u32)))
            .unwrap(),
        digest(sequence as u8),
    )
}

fn workflow(status: SurfaceWorkflowStatus) -> SurfaceWorkflow {
    SurfaceWorkflow {
        workflow_run_id: SurfaceWorkflowRunId::try_new("manifest-workflow").unwrap(),
        task_id: SurfaceTaskId::try_new("workflow-task").unwrap(),
        revision: WorkflowRevision::try_new(1).unwrap(),
        name: NonEmptyText::try_new("manifest workflow").unwrap(),
        status,
        phases: Vec::new(),
        agents: Vec::new(),
        result: None,
        error: None,
        parent: None,
    }
}

fn workflow_status_name(status: SurfaceWorkflowStatus) -> &'static str {
    match status {
        SurfaceWorkflowStatus::Queued => "Queued",
        SurfaceWorkflowStatus::Running => "Running",
        SurfaceWorkflowStatus::Paused => "Paused",
        SurfaceWorkflowStatus::Stopping => "Stopping",
        SurfaceWorkflowStatus::Stopped => "Stopped",
        SurfaceWorkflowStatus::Completed => "Completed",
        SurfaceWorkflowStatus::Failed => "Failed",
        SurfaceWorkflowStatus::Cancelled => "Cancelled",
        SurfaceWorkflowStatus::AsyncLaunched => "AsyncLaunched",
    }
}

fn all_workflow_statuses() -> [SurfaceWorkflowStatus; 9] {
    [
        SurfaceWorkflowStatus::Queued,
        SurfaceWorkflowStatus::Running,
        SurfaceWorkflowStatus::Paused,
        SurfaceWorkflowStatus::Stopping,
        SurfaceWorkflowStatus::Stopped,
        SurfaceWorkflowStatus::Completed,
        SurfaceWorkflowStatus::Failed,
        SurfaceWorkflowStatus::Cancelled,
        SurfaceWorkflowStatus::AsyncLaunched,
    ]
}

fn workflow_transition_result(
    source: Option<SurfaceWorkflowStatus>,
    target: SurfaceWorkflowStatus,
    seed: u32,
) -> SurfaceReduceResult {
    let mut snapshot = snapshot();
    if let Some(source) = source {
        snapshot.workflows.push(workflow(source));
    }
    let state = SurfaceReducerState::new(snapshot);
    let fence = SurfaceWorkflowFence {
        workflow_run_id: SurfaceWorkflowRunId::try_new("manifest-workflow").unwrap(),
        workflow_revision: WorkflowRevision::try_new(1).unwrap(),
        parent: None,
    };
    let patch = if source.is_none() {
        WorkflowPatch::Started {
            workflow: workflow(target),
        }
    } else {
        let next_revision = WorkflowRevision::try_new(2).unwrap();
        match target {
            SurfaceWorkflowStatus::Running => WorkflowPatch::Resumed {
                fence,
                next_revision,
            },
            SurfaceWorkflowStatus::Paused => WorkflowPatch::Paused {
                fence,
                next_revision,
                reason: DisplayText::new("pause"),
            },
            SurfaceWorkflowStatus::Stopping => WorkflowPatch::Stopping {
                fence,
                next_revision,
                reason: DisplayText::new("stop requested"),
            },
            SurfaceWorkflowStatus::Stopped => WorkflowPatch::Stopped {
                fence,
                next_revision,
                reason: DisplayText::new("stopped"),
            },
            SurfaceWorkflowStatus::Completed => WorkflowPatch::Completed {
                fence,
                next_revision,
            },
            SurfaceWorkflowStatus::Failed => WorkflowPatch::Failed {
                fence,
                next_revision,
                error: DisplayText::new("failed"),
            },
            SurfaceWorkflowStatus::Cancelled => WorkflowPatch::Cancelled {
                fence,
                next_revision,
                reason: DisplayText::new("cancelled"),
            },
            SurfaceWorkflowStatus::AsyncLaunched => WorkflowPatch::AsyncLaunched {
                fence,
                next_revision,
            },
            SurfaceWorkflowStatus::Queued => WorkflowPatch::Resumed {
                fence,
                next_revision,
            },
        }
    };
    let batch = batch(
        &state,
        seed,
        vec![(SurfaceScope::Thread, SurfaceEvent::Workflow(patch))],
    );
    reduce_batch(SurfaceReduceMode::Live, &state, &batch)
}

#[test]
fn workflow_run_transitions_are_generated_from_manifest_and_include_stopping() {
    let allowed = manifest_pairs("workflow_run_status_transitions");
    assert!(allowed.contains(&("Running".to_owned(), "Stopping".to_owned())));
    assert!(allowed.contains(&("Paused".to_owned(), "Stopping".to_owned())));
    assert!(allowed.contains(&("AsyncLaunched".to_owned(), "Stopping".to_owned())));
    assert!(allowed.contains(&("Stopping".to_owned(), "Stopped".to_owned())));
    assert!(!allowed.contains(&("Running".to_owned(), "Stopped".to_owned())));

    let mut seed = 7_000;
    for target in all_workflow_statuses() {
        let pair = ("Absent".to_owned(), workflow_status_name(target).to_owned());
        let result = workflow_transition_result(None, target, seed);
        seed += 1;
        if allowed.contains(&pair) {
            assert_eq!(applied(result).snapshot().workflows[0].status, target);
        } else {
            rejected(result, SurfaceReducerErrorCode::IllegalTransition);
        }
    }
    for source in all_workflow_statuses() {
        for target in all_workflow_statuses() {
            if target == SurfaceWorkflowStatus::Queued {
                assert!(
                    !allowed
                        .contains(&(workflow_status_name(source).to_owned(), "Queued".to_owned()))
                );
                continue;
            }
            let pair = (
                workflow_status_name(source).to_owned(),
                workflow_status_name(target).to_owned(),
            );
            let result = workflow_transition_result(Some(source), target, seed);
            seed += 1;
            if allowed.contains(&pair) {
                let state = applied(result);
                assert_eq!(state.snapshot().workflows[0].status, target);
                assert_eq!(state.snapshot().workflows[0].revision.get(), 2);
            } else {
                rejected(result, SurfaceReducerErrorCode::IllegalTransition);
            }
        }
    }

    for terminal in [
        SurfaceWorkflowStatus::Stopped,
        SurfaceWorkflowStatus::Completed,
        SurfaceWorkflowStatus::Failed,
        SurfaceWorkflowStatus::Cancelled,
    ] {
        for target in all_workflow_statuses() {
            assert!(!allowed.contains(&(
                workflow_status_name(terminal).to_owned(),
                workflow_status_name(target).to_owned()
            )));
        }
    }
}

fn workflow_phase(name: &str, status: SurfaceWorkflowStatus) -> SurfaceWorkflowPhase {
    SurfaceWorkflowPhase {
        name: NonEmptyText::try_new(name).unwrap(),
        status,
        started_at: Some(UnixMillis::new(1)),
        completed_at: None,
        agent_count: 0,
        summary: None,
        error: None,
    }
}

fn phase_transition_result(
    source: Option<SurfaceWorkflowStatus>,
    target: SurfaceWorkflowStatus,
    seed: u32,
) -> SurfaceReduceResult {
    let mut snapshot = snapshot();
    let mut workflow = workflow(SurfaceWorkflowStatus::Running);
    if let Some(source) = source {
        workflow.phases.push(workflow_phase("phase-1", source));
    }
    snapshot.workflows.push(workflow);
    let state = SurfaceReducerState::new(snapshot);
    let fence = SurfaceWorkflowFence {
        workflow_run_id: SurfaceWorkflowRunId::try_new("manifest-workflow").unwrap(),
        workflow_revision: WorkflowRevision::try_new(1).unwrap(),
        parent: None,
    };
    let phase = workflow_phase("phase-1", target);
    let patch = if source.is_none() {
        WorkflowPatch::PhaseStarted {
            fence,
            next_revision: WorkflowRevision::try_new(2).unwrap(),
            phase,
        }
    } else {
        WorkflowPatch::PhaseCompleted {
            fence,
            next_revision: WorkflowRevision::try_new(2).unwrap(),
            phase,
        }
    };
    let batch = batch(
        &state,
        seed,
        vec![(SurfaceScope::Thread, SurfaceEvent::Workflow(patch))],
    );
    reduce_batch(SurfaceReduceMode::Live, &state, &batch)
}

#[test]
fn workflow_phase_transitions_are_generated_from_manifest() {
    let allowed = manifest_pairs("workflow_phase_status_transitions");
    let statuses = [
        SurfaceWorkflowStatus::Running,
        SurfaceWorkflowStatus::Completed,
        SurfaceWorkflowStatus::Failed,
        SurfaceWorkflowStatus::Stopped,
        SurfaceWorkflowStatus::Cancelled,
    ];
    let mut seed = 8_000;
    for target in statuses {
        let pair = ("Absent".to_owned(), workflow_status_name(target).to_owned());
        let result = phase_transition_result(None, target, seed);
        seed += 1;
        if allowed.contains(&pair) {
            assert_eq!(
                applied(result).snapshot().workflows[0].phases[0].status,
                target
            );
        } else {
            rejected(result, SurfaceReducerErrorCode::IllegalTransition);
        }
    }
    for source in statuses {
        for target in statuses {
            let pair = (
                workflow_status_name(source).to_owned(),
                workflow_status_name(target).to_owned(),
            );
            let result = phase_transition_result(Some(source), target, seed);
            seed += 1;
            if allowed.contains(&pair) {
                assert_eq!(
                    applied(result).snapshot().workflows[0].phases[0].status,
                    target
                );
            } else {
                rejected(result, SurfaceReducerErrorCode::IllegalTransition);
            }
        }
    }
}

fn agent_status_name(status: SurfaceWorkflowAgentStatus) -> &'static str {
    match status {
        SurfaceWorkflowAgentStatus::Pending => "Pending",
        SurfaceWorkflowAgentStatus::Running => "Running",
        SurfaceWorkflowAgentStatus::Cached => "Cached",
        SurfaceWorkflowAgentStatus::Completed => "Completed",
        SurfaceWorkflowAgentStatus::Failed => "Failed",
        SurfaceWorkflowAgentStatus::Cancelled => "Cancelled",
    }
}

fn workflow_agent(status: SurfaceWorkflowAgentStatus) -> SurfaceWorkflowAgent {
    SurfaceWorkflowAgent {
        agent_id: SurfaceSubagentId::try_new("workflow-agent").unwrap(),
        phase: NonEmptyText::try_new("phase-1").unwrap(),
        status,
        attempt: 0,
        output: None,
        error: None,
        usage: None,
    }
}

fn agent_transition_result(
    source: Option<SurfaceWorkflowAgentStatus>,
    target: SurfaceWorkflowAgentStatus,
    seed: u32,
) -> SurfaceReduceResult {
    let mut snapshot = snapshot();
    let mut workflow = workflow(SurfaceWorkflowStatus::Running);
    if let Some(source) = source {
        workflow.agents.push(workflow_agent(source));
    }
    snapshot.workflows.push(workflow);
    let state = SurfaceReducerState::new(snapshot);
    let fence = SurfaceWorkflowFence {
        workflow_run_id: SurfaceWorkflowRunId::try_new("manifest-workflow").unwrap(),
        workflow_revision: WorkflowRevision::try_new(1).unwrap(),
        parent: None,
    };
    let agent = workflow_agent(target);
    let next_revision = WorkflowRevision::try_new(2).unwrap();
    let patch = match target {
        SurfaceWorkflowAgentStatus::Pending | SurfaceWorkflowAgentStatus::Running => {
            WorkflowPatch::AgentStarted {
                fence,
                next_revision,
                agent,
            }
        }
        SurfaceWorkflowAgentStatus::Cached => WorkflowPatch::AgentCached {
            fence,
            next_revision,
            agent,
        },
        SurfaceWorkflowAgentStatus::Completed => WorkflowPatch::AgentCompleted {
            fence,
            next_revision,
            agent,
        },
        SurfaceWorkflowAgentStatus::Failed => WorkflowPatch::AgentFailed {
            fence,
            next_revision,
            agent,
        },
        SurfaceWorkflowAgentStatus::Cancelled => WorkflowPatch::AgentCancelled {
            fence,
            next_revision,
            agent,
        },
    };
    let batch = batch(
        &state,
        seed,
        vec![(SurfaceScope::Thread, SurfaceEvent::Workflow(patch))],
    );
    reduce_batch(SurfaceReduceMode::Live, &state, &batch)
}

#[test]
fn workflow_agent_attempt_transitions_are_generated_from_manifest() {
    let allowed = manifest_pairs("workflow_agent_attempt_transitions");
    let statuses = [
        SurfaceWorkflowAgentStatus::Pending,
        SurfaceWorkflowAgentStatus::Running,
        SurfaceWorkflowAgentStatus::Cached,
        SurfaceWorkflowAgentStatus::Completed,
        SurfaceWorkflowAgentStatus::Failed,
        SurfaceWorkflowAgentStatus::Cancelled,
    ];
    let mut seed = 9_000;
    for target in statuses {
        let pair = ("Absent".to_owned(), agent_status_name(target).to_owned());
        let result = agent_transition_result(None, target, seed);
        seed += 1;
        if allowed.contains(&pair) {
            assert_eq!(
                applied(result).snapshot().workflows[0].agents[0].status,
                target
            );
        } else {
            rejected(result, SurfaceReducerErrorCode::IllegalTransition);
        }
    }
    for source in statuses {
        for target in statuses {
            let pair = (
                agent_status_name(source).to_owned(),
                agent_status_name(target).to_owned(),
            );
            let result = agent_transition_result(Some(source), target, seed);
            seed += 1;
            if allowed.contains(&pair) {
                assert_eq!(
                    applied(result).snapshot().workflows[0].agents[0].status,
                    target
                );
            } else {
                rejected(result, SurfaceReducerErrorCode::IllegalTransition);
            }
        }
    }
}

fn subagent(status: SurfaceSubagentStatus, revision: u64) -> SurfaceSubagent {
    SurfaceSubagent {
        subagent_id: SurfaceSubagentId::try_new("manifest-subagent").unwrap(),
        task_id: SurfaceTaskId::try_new("manifest-task").unwrap(),
        revision: SubagentRevision::try_new(revision).unwrap(),
        description: DisplayText::new("manifest subagent"),
        status,
        activity: None,
        turn: None,
        usage: None,
        output: None,
        error: None,
        owner: subagent_owner(),
        source: subagent_source(revision),
    }
}

fn subagent_status_name(status: SurfaceSubagentStatus) -> &'static str {
    match status {
        SurfaceSubagentStatus::Running => "Running",
        SurfaceSubagentStatus::Completed => "Completed",
        SurfaceSubagentStatus::Failed => "Failed",
        SurfaceSubagentStatus::Cancelled => "Cancelled",
    }
}

#[test]
fn subagent_transitions_are_generated_from_manifest_and_terminals_absorb() {
    let allowed = manifest_pairs("subagent_status_transitions");
    let statuses = [
        SurfaceSubagentStatus::Running,
        SurfaceSubagentStatus::Completed,
        SurfaceSubagentStatus::Failed,
        SurfaceSubagentStatus::Cancelled,
    ];
    let mut seed = 10_000;
    for source in [None, Some(SurfaceSubagentStatus::Running)] {
        for target in statuses {
            let mut snapshot = snapshot();
            if let Some(source) = source {
                snapshot.subagents.push(subagent(source, 1));
            }
            let state = SurfaceReducerState::new(snapshot);
            let patch = match (source, target) {
                (None, SurfaceSubagentStatus::Running) => SubagentPatch::Started {
                    expected_revision: ExpectedAbsentSubagentRevision,
                    subagent: RunningSurfaceSubagent::try_new(subagent(target, 1)).unwrap(),
                },
                (Some(SurfaceSubagentStatus::Running), SurfaceSubagentStatus::Running) => {
                    SubagentPatch::Progress {
                        subagent_id: SurfaceSubagentId::try_new("manifest-subagent").unwrap(),
                        expected_revision: SubagentRevision::try_new(1).unwrap(),
                        next_revision: SubagentRevision::try_new(2).unwrap(),
                        owner: subagent_owner(),
                        source: subagent_source(2),
                        activity: DisplayText::new("progress"),
                        turn: Some(1),
                        usage: None,
                    }
                }
                (_, terminal) => SubagentPatch::Completed {
                    subagent_id: SurfaceSubagentId::try_new("manifest-subagent").unwrap(),
                    expected_revision: SubagentRevision::try_new(1).unwrap(),
                    next_revision: SubagentRevision::try_new(2).unwrap(),
                    owner: subagent_owner(),
                    source: subagent_source(2),
                    status: match terminal {
                        SurfaceSubagentStatus::Completed => {
                            SurfaceSubagentTerminalStatus::Completed
                        }
                        SurfaceSubagentStatus::Failed => SurfaceSubagentTerminalStatus::Failed,
                        SurfaceSubagentStatus::Cancelled => {
                            SurfaceSubagentTerminalStatus::Cancelled
                        }
                        SurfaceSubagentStatus::Running => SurfaceSubagentTerminalStatus::Failed,
                    },
                    output: None,
                    error: None,
                    usage: None,
                },
            };
            let pair = (
                source
                    .map(subagent_status_name)
                    .unwrap_or("Absent")
                    .to_owned(),
                if source == Some(SurfaceSubagentStatus::Running)
                    && target == SurfaceSubagentStatus::Running
                {
                    "Running(Progress)".to_owned()
                } else {
                    subagent_status_name(target).to_owned()
                },
            );
            let batch = batch(
                &state,
                seed,
                vec![(
                    SurfaceScope::Generation {
                        fence: operation_fence(),
                    },
                    SurfaceEvent::Subagent(patch),
                )],
            );
            seed += 1;
            let result = reduce_batch(SurfaceReduceMode::Live, &state, &batch);
            if allowed.contains(&pair) {
                assert_eq!(applied(result).snapshot().subagents[0].status, target);
            } else {
                rejected(result, SurfaceReducerErrorCode::IllegalTransition);
            }
        }
    }

    for terminal in [
        SurfaceSubagentStatus::Completed,
        SurfaceSubagentStatus::Failed,
        SurfaceSubagentStatus::Cancelled,
    ] {
        let mut snapshot = snapshot();
        snapshot.subagents.push(subagent(terminal, 1));
        let state = SurfaceReducerState::new(snapshot);
        let patch = SubagentPatch::Progress {
            subagent_id: SurfaceSubagentId::try_new("manifest-subagent").unwrap(),
            expected_revision: SubagentRevision::try_new(1).unwrap(),
            next_revision: SubagentRevision::try_new(2).unwrap(),
            owner: subagent_owner(),
            source: subagent_source(2),
            activity: DisplayText::new("illegal reopen"),
            turn: None,
            usage: None,
        };
        let batch = batch(
            &state,
            seed,
            vec![(
                SurfaceScope::Generation {
                    fence: operation_fence(),
                },
                SurfaceEvent::Subagent(patch),
            )],
        );
        seed += 1;
        rejected(
            reduce_batch(SurfaceReduceMode::Live, &state, &batch),
            SurfaceReducerErrorCode::IllegalTransition,
        );
    }
}

fn task_status_name(status: SurfaceTaskStatus) -> &'static str {
    match status {
        SurfaceTaskStatus::Queued => "Queued",
        SurfaceTaskStatus::Running => "Running",
        SurfaceTaskStatus::Paused => "Paused",
        SurfaceTaskStatus::Stopping => "Stopping",
        SurfaceTaskStatus::Stopped => "Stopped",
        SurfaceTaskStatus::Completed => "Completed",
        SurfaceTaskStatus::Failed => "Failed",
        SurfaceTaskStatus::ApprovalRequired => "ApprovalRequired",
        SurfaceTaskStatus::Cancelled => "Cancelled",
    }
}

fn all_task_statuses() -> [SurfaceTaskStatus; 9] {
    [
        SurfaceTaskStatus::Queued,
        SurfaceTaskStatus::Running,
        SurfaceTaskStatus::Paused,
        SurfaceTaskStatus::Stopping,
        SurfaceTaskStatus::Stopped,
        SurfaceTaskStatus::Completed,
        SurfaceTaskStatus::Failed,
        SurfaceTaskStatus::ApprovalRequired,
        SurfaceTaskStatus::Cancelled,
    ]
}

fn task_transition_result(
    source: Option<SurfaceTaskStatus>,
    target: SurfaceTaskStatus,
    seed: u32,
) -> SurfaceReduceResult {
    let mut snapshot = snapshot();
    if let Some(source) = source {
        snapshot.tasks.push(task(source, 1));
    }
    let state = SurfaceReducerState::new(snapshot);
    let patch = if source.is_none() {
        TaskPatch::Upserted {
            expected_revision: None,
            task: task(target, 1),
        }
    } else {
        TaskPatch::StatusChanged {
            task_id: SurfaceTaskId::try_new("manifest-task").unwrap(),
            expected_revision: TaskRevision::try_new(1).unwrap(),
            next_revision: TaskRevision::try_new(2).unwrap(),
            status: target,
            completed_at: None,
            result: None,
            error: None,
        }
    };
    let batch = batch(
        &state,
        seed,
        vec![(SurfaceScope::Thread, SurfaceEvent::Task(patch))],
    );
    reduce_batch(SurfaceReduceMode::Live, &state, &batch)
}

#[test]
fn task_transitions_are_generated_from_manifest_and_terminals_absorb() {
    let allowed = manifest_pairs("task_status_transitions");
    assert_eq!(allowed.len(), 28);
    let mut seed = 1_000_u32;
    for target in all_task_statuses() {
        let pair = ("Absent".to_owned(), task_status_name(target).to_owned());
        let result = task_transition_result(None, target, seed);
        seed += 1;
        if allowed.contains(&pair) {
            let state = applied(result);
            assert_eq!(state.snapshot().tasks[0].status, target);
        } else {
            rejected(result, SurfaceReducerErrorCode::IllegalTransition);
        }
    }
    for source in all_task_statuses() {
        for target in all_task_statuses() {
            let pair = (
                task_status_name(source).to_owned(),
                task_status_name(target).to_owned(),
            );
            let result = task_transition_result(Some(source), target, seed);
            seed += 1;
            if allowed.contains(&pair) {
                let state = applied(result);
                assert_eq!(state.snapshot().tasks[0].status, target);
                assert_eq!(state.snapshot().tasks[0].revision.get(), 2);
            } else {
                rejected(result, SurfaceReducerErrorCode::IllegalTransition);
            }
        }
    }

    let terminals = [
        SurfaceTaskStatus::Stopped,
        SurfaceTaskStatus::Completed,
        SurfaceTaskStatus::Failed,
        SurfaceTaskStatus::Cancelled,
    ];
    for terminal in terminals {
        for target in all_task_statuses() {
            assert!(!allowed.contains(&(
                task_status_name(terminal).to_owned(),
                task_status_name(target).to_owned()
            )));
        }
    }
}

#[test]
fn task_revision_must_be_exact_and_contiguous() {
    let mut snapshot = snapshot();
    snapshot.tasks.push(task(SurfaceTaskStatus::Running, 3));
    let state = SurfaceReducerState::new(snapshot);
    for (expected, next) in [(2, 4), (3, 5), (4, 5)] {
        let patch = TaskPatch::StatusChanged {
            task_id: SurfaceTaskId::try_new("manifest-task").unwrap(),
            expected_revision: TaskRevision::try_new(expected).unwrap(),
            next_revision: TaskRevision::try_new(next).unwrap(),
            status: SurfaceTaskStatus::Paused,
            completed_at: None,
            result: None,
            error: None,
        };
        let batch = batch(
            &state,
            (5_000 + expected + next) as u32,
            vec![(SurfaceScope::Thread, SurfaceEvent::Task(patch))],
        );
        rejected(
            reduce_batch(SurfaceReduceMode::Live, &state, &batch),
            SurfaceReducerErrorCode::StaleRevision,
        );
    }
}

#[test]
fn manifest_contains_every_required_reducer_inventory() {
    for key in [
        "task_status_transitions",
        "workflow_run_status_transitions",
        "workflow_phase_status_transitions",
        "workflow_agent_attempt_transitions",
        "subagent_status_transitions",
    ] {
        assert!(!manifest_pairs(key).is_empty(), "missing {key}");
    }
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    assert_eq!(
        manifest["goal_continuation_retry_contract"]["key"],
        "predecessor.operation_fence"
    );
    assert_eq!(
        manifest["closed_inventory"]["surface_commit_batch_limits"]["event_limit"],
        SURFACE_COMMIT_BATCH_EVENT_LIMIT
    );
    assert_eq!(
        manifest["closed_inventory"]["surface_commit_batch_limits"]["canonical_encoded_byte_limit"],
        SURFACE_COMMIT_BATCH_BYTE_LIMIT
    );
    assert_eq!(BTreeSet::from(["Live", "Rematerialization"]).len(), 2);
}

fn reservation_lease(operation_id: &SurfaceOperationId) -> ReservationLease {
    serde_json::from_value(serde_json::json!({
        "lease_id": SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(11_001)).unwrap(),
        "operation_id": operation_id,
        "reservation_sequence": 1,
        "issuing_host_incarnation": HostIncarnation::try_from_bytes(uuid_v7_bytes(11_002)).unwrap(),
        "issued_at": {
            "clock_id": HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(11_003)).unwrap(),
            "tick": 1
        },
        "duration": SURFACE_RESERVATION_LEASE_MS
    }))
    .unwrap()
}

fn replayability() -> Replayability {
    Replayability::NonReplayable {
        reason: NonReplayableReason::HistoryDisabled,
        live_capsule: LiveOperationCapsule::Available {
            incarnation: incarnation(),
        },
    }
}

fn operation_record() -> OperationRecord {
    let operation_id = SurfaceOperationId::try_from_bytes(uuid_v7_bytes(11_000)).unwrap();
    OperationRecord {
        operation_id: operation_id.clone(),
        request_id: SurfaceRequestId::try_from_bytes(uuid_v7_bytes(11_004)).unwrap(),
        intent: OperationIntent {
            origin: OperationOrigin::TuiUser,
            kind: OperationKind::ManualCompaction {
                reason: ManualCompactionReason::Manual,
            },
            initial_replayability: replayability(),
            busy_disposition: BusyDisposition::Queue,
            interrupt_settlement: InterruptSettlement::SuspendUntilExplicitControl,
            legacy_visibility: LegacyVisibility::PublishAfterAdmitted,
            settings_revision: SettingsRevision::try_new(1).unwrap(),
            policy_epoch: PolicyEpoch::try_new(1).unwrap(),
            required_capabilities: BTreeSet::new(),
            capability_fingerprint: digest(70),
            settings_receipt: OperationSettingsPreparationReceipt::Current {
                settings_revision: SettingsRevision::try_new(1).unwrap(),
                policy_epoch: PolicyEpoch::try_new(1).unwrap(),
            },
        },
        phase: OperationPhase::Requested,
        reservation: reservation_lease(&operation_id),
        ready_for_admission: true,
        initial_logical_turn_id: None,
        initial_input_item_id: None,
        generations: Vec::new(),
        agent_loop_turns: Vec::new(),
        pending_control: None,
        finalization: None,
        terminal: None,
    }
}

fn generation(operation: &OperationRecord) -> GenerationRecord {
    GenerationRecord {
        fence: SurfaceOperationFence {
            thread_id: thread_id(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: operation.operation_id.clone(),
            generation_id: SurfaceGenerationId::new(0),
        },
        logical_turn_id: SurfaceTurnId::new(),
        input: GenerationInputState::NotApplicable,
        predecessor: None,
        attempt: GenerationAttempt::Initial,
        goal_identity: None,
        replayability: replayability(),
        required_capabilities: BTreeSet::new(),
        capability_fingerprint: digest(70),
        phase: GenerationPhase::Reserved,
        started_witness: None,
        stop_reason: None,
    }
}

fn operation_scope(operation: &OperationRecord) -> SurfaceScope {
    SurfaceScope::Operation {
        operation_id: operation.operation_id.clone(),
    }
}

fn generation_scope(generation: &GenerationRecord) -> SurfaceScope {
    SurfaceScope::Generation {
        fence: generation.fence.clone(),
    }
}

fn manifest_string_inventory(key: &str) -> Vec<String> {
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    manifest[key]
        .as_array()
        .unwrap_or_else(|| panic!("manifest {key} is not an array"))
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("manifest {key}[{index}] is not a string"))
                .to_owned()
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManifestTransitionRow {
    source: String,
    target: String,
    event: String,
    invariants: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManifestOperationTerminalRow {
    source: String,
    target: String,
    condition: String,
}

fn manifest_operation_terminal_rows() -> Vec<ManifestOperationTerminalRow> {
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    manifest["operation_terminal_mapping"]
        .as_array()
        .expect("operation_terminal_mapping is not an array")
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let values = row
                .as_array()
                .unwrap_or_else(|| panic!("operation_terminal_mapping[{index}] is not an array"));
            assert_eq!(
                values.len(),
                3,
                "operation_terminal_mapping[{index}] has an unexpected width"
            );
            let cell = |cell_index: usize| {
                values[cell_index]
                    .as_str()
                    .unwrap_or_else(|| {
                        panic!("operation_terminal_mapping[{index}][{cell_index}] is not a string")
                    })
                    .to_owned()
            };
            ManifestOperationTerminalRow {
                source: cell(0),
                target: cell(1),
                condition: cell(2),
            }
        })
        .collect()
}

fn manifest_transition_rows(key: &str) -> Vec<ManifestTransitionRow> {
    let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    manifest[key]
        .as_array()
        .unwrap_or_else(|| panic!("manifest {key} is not an array"))
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let values = row
                .as_array()
                .unwrap_or_else(|| panic!("manifest {key}[{index}] is not an array"));
            assert_eq!(
                values.len(),
                4,
                "manifest {key}[{index}] has an unexpected width"
            );
            let cell = |cell_index: usize| {
                values[cell_index]
                    .as_str()
                    .unwrap_or_else(|| {
                        panic!("manifest {key}[{index}][{cell_index}] is not a string")
                    })
                    .to_owned()
            };
            let invariants = values[3]
                .as_array()
                .unwrap_or_else(|| panic!("manifest {key}[{index}][3] is not an array"))
                .iter()
                .enumerate()
                .map(|(invariant_index, invariant)| {
                    invariant.as_str().unwrap_or_else(|| {
                        panic!("manifest {key}[{index}][3][{invariant_index}] is not a string")
                    })
                })
                .map(str::to_owned)
                .collect();
            ManifestTransitionRow {
                source: cell(0),
                target: cell(1),
                event: cell(2),
                invariants,
            }
        })
        .collect()
}

fn operation_state(operation: OperationRecord, queued: bool) -> SurfaceReducerState {
    let mut initial = snapshot();
    if queued {
        initial.queued_operations.push(operation);
    } else {
        initial.foreground_operation = Some(operation);
    }
    SurfaceReducerState::new(initial)
}

fn generation_started_witness(
    seed: u32,
    replayability: &Replayability,
) -> GenerationStartedWitness {
    GenerationStartedWitness {
        started_commit_id: recorded_commit_id(seed),
        settings_revision: SettingsRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        durable_replayability_digest: canonical_replayability_digest(replayability),
        capability_fingerprint: digest(70),
    }
}

fn generation_at(
    operation: &OperationRecord,
    generation_id: u64,
    phase: GenerationPhase,
    stop_reason: Option<GenerationStopReason>,
) -> GenerationRecord {
    let mut generation = generation(operation);
    generation.fence.generation_id = SurfaceGenerationId::new(generation_id);
    generation.phase = phase;
    generation.predecessor = (generation_id > 0).then(|| SurfaceOperationFence {
        generation_id: SurfaceGenerationId::new(generation_id - 1),
        ..generation.fence.clone()
    });
    generation.started_witness = matches!(
        phase,
        GenerationPhase::Started | GenerationPhase::Transferred
    )
    .then(|| generation_started_witness(20_000 + generation_id as u32, &generation.replayability));
    generation.stop_reason = stop_reason;
    generation
}

fn background_fence(fence: &SurfaceOperationFence, token: u8) -> SurfaceBackgroundFence {
    // The public surface intentionally exposes no opaque-token constructor. Keep this
    // integration-only fixture local; transmute also compile-checks the 32-byte layout.
    let background_owner_token =
        unsafe { std::mem::transmute::<[u8; 32], SurfaceBackgroundOwnerToken>([token; 32]) };
    SurfaceBackgroundFence {
        operation_fence: fence.clone(),
        background_owner_token,
    }
}

fn safe_message(value: &str) -> SafeDiagnosticText {
    SafeDiagnosticText::try_new(value).unwrap()
}

fn vector_usage() -> UsageTotals {
    UsageTotals {
        input_tokens: 11,
        output_tokens: 7,
        cache_tokens: 3,
        estimated_cost_usd_micros: 19,
    }
}

fn recorded_commit_id(seed: u32) -> SurfaceCommitId {
    match commit_class(seed) {
        CommitClass::Recorded { commit_id, .. } => commit_id,
        CommitClass::Ephemeral { .. } => unreachable!(),
    }
}

fn finalization_record(
    operation: &OperationRecord,
    seed: u32,
    selected_cause: OperationFinalizationCause,
    suspended_cause: Option<SuspendedFinalizationCause>,
    expected_settlements: Vec<SurfaceSettlementId>,
    settled: Vec<SurfaceSettlementReceipt>,
) -> OperationFinalizationRecord {
    let finalize_intent_id =
        SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
    let terminal_commit_id = recorded_commit_id(seed);
    OperationFinalizationRecord {
        finalize_intent_id: finalize_intent_id.clone(),
        terminal_commit_id: terminal_commit_id.clone(),
        started_at: FinalizationStartedAtCursor {
            operation_id: operation.operation_id.clone(),
            finalize_intent_id,
            terminal_commit_id,
            event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(seed + 2)).unwrap(),
            cursor: cursor(0),
            commit_class: commit_class(seed),
            batch_digest: digest((seed % 251) as u8),
        },
        selected_cause,
        suspended_cause,
        expected_settlements,
        settled,
    }
}

fn assert_vector_applied(label: &str, result: SurfaceReduceResult) -> SurfaceReducerState {
    match result {
        SurfaceReduceResult::Applied { state } => state,
        SurfaceReduceResult::Rejected { error } => panic!(
            "manifest vector {label} did not apply: {:?}: {}",
            error.code,
            error.message.as_str()
        ),
        SurfaceReduceResult::AlreadyApplied { .. } => {
            panic!("manifest vector {label} unexpectedly replayed")
        }
    }
}

fn assert_operation_vector_applied(
    label: &str,
    failures: &mut Vec<String>,
    initial: &SurfaceReducerState,
    positive: &SurfaceCommitBatch,
    operation_id: &SurfaceOperationId,
) {
    assert_vector_applied(
        label,
        reduce_batch(SurfaceReduceMode::Live, initial, positive),
    );

    let mut omitted_snapshot = initial.snapshot().clone();
    let operation = omitted_snapshot
        .queued_operations
        .iter_mut()
        .chain(omitted_snapshot.foreground_operation.iter_mut())
        .chain(omitted_snapshot.operation_history.iter_mut())
        .find(|operation| operation.operation_id == *operation_id)
        .unwrap_or_else(|| panic!("manifest vector {label} lacks its source operation"));
    operation.phase = OperationPhase::Terminal;
    let omitted_state = SurfaceReducerState::new(omitted_snapshot);
    push_if_not_rejected(
        failures,
        format!("{label}:omitted_edge"),
        reduce_batch(SurfaceReduceMode::Live, &omitted_state, positive),
    );
}

fn terminal_record_for(
    operation: &OperationRecord,
    terminal: OperationTerminal,
    usage: UsageTotals,
    settlement_receipts: Vec<SurfaceSettlementReceipt>,
) -> OperationTerminalRecord {
    OperationTerminalRecord {
        operation_id: operation.operation_id.clone(),
        finalize_intent_id: operation
            .finalization
            .as_ref()
            .unwrap()
            .finalize_intent_id
            .clone(),
        terminal,
        usage,
        source_diagnostic_digest: None,
        settlement_receipts,
        completion_proof: SurfaceOperationCompletionProof::unverified(
            "test terminal has no verifier proof",
        ),
        committed_at: UnixMillis::new(5),
    }
}

struct OperationTerminalFixture {
    operation: OperationRecord,
    selected_cause: OperationFinalizationCause,
    suspended_cause: Option<SuspendedFinalizationCause>,
    terminal: OperationTerminal,
    expected_settlements: Vec<SurfaceSettlementId>,
    settlement_receipts: Vec<SurfaceSettlementReceipt>,
}

fn noncurrent_replayability() -> Replayability {
    Replayability::NonReplayable {
        reason: NonReplayableReason::Missing,
        live_capsule: LiveOperationCapsule::Unavailable,
    }
}

fn generation_terminal_fixture(
    reason: GenerationStopReason,
    terminal: OperationTerminal,
    replayability: Replayability,
) -> OperationTerminalFixture {
    let mut operation = operation_record();
    operation.phase = OperationPhase::Admitted;
    operation.intent.initial_replayability = replayability.clone();
    let mut generation = generation_at(
        &operation,
        0,
        GenerationPhase::Stopped,
        Some(reason.clone()),
    );
    generation.replayability = replayability;
    operation.generations.push(generation);
    OperationTerminalFixture {
        operation,
        selected_cause: OperationFinalizationCause::GenerationStop(reason),
        suspended_cause: None,
        terminal,
        expected_settlements: Vec::new(),
        settlement_receipts: Vec::new(),
    }
}

fn terminalization_terminal(cause: TerminalizationCause) -> OperationTerminal {
    match cause {
        TerminalizationCause::UserCancel => OperationTerminal::Cancelled {
            reason: CancelReason::User,
        },
        TerminalizationCause::GoalPause => OperationTerminal::Cancelled {
            reason: CancelReason::GoalPause,
        },
        TerminalizationCause::HostShutdown => OperationTerminal::Shutdown {
            reason: SurfaceShutdownReason::HostShutdown,
        },
        TerminalizationCause::ThreadClose => OperationTerminal::Shutdown {
            reason: SurfaceShutdownReason::ThreadClose,
        },
    }
}

fn budget_for_terminal_source(source: &str, seed: u32) -> Option<OperationBudget> {
    match source {
        "Completed(BudgetExhausted(ModelTokens))" => Some(OperationBudget::ModelTokens {
            limit: Some(100),
            observed: Some(100),
        }),
        "Completed(BudgetExhausted(TurnRequests(AgentLoop)))" => {
            Some(OperationBudget::TurnRequests {
                scope: TurnRequestBudgetScope::AgentLoop,
                limit: 12,
                observed: 12,
            })
        }
        "Completed(BudgetExhausted(TurnRequests(Subagent)))" => {
            Some(OperationBudget::TurnRequests {
                scope: TurnRequestBudgetScope::Subagent,
                limit: 7,
                observed: 7,
            })
        }
        "Completed(BudgetExhausted(GoalTokenBudget))" => Some(OperationBudget::GoalTokenBudget {
            goal_id: SurfaceGoalId::try_new(format!("terminal-goal-{seed}")).unwrap(),
            limit: 1_000,
            observed: 1_000,
        }),
        "Completed(BudgetExhausted(WorkflowTokenBudget))" => {
            Some(OperationBudget::WorkflowTokenBudget {
                workflow_run_id: SurfaceWorkflowRunId::try_new(format!("terminal-workflow-{seed}"))
                    .unwrap(),
                limit: 2_000,
                observed: 2_000,
            })
        }
        "Completed(BudgetExhausted(MonetaryBudgetUsdMicros))" => {
            Some(OperationBudget::MonetaryBudgetUsdMicros {
                limit: 3_000,
                observed: 3_000,
            })
        }
        _ => None,
    }
}

fn execution_failure_class_for_source(source: &str) -> Option<GenerationExecutionFailureClass> {
    match source {
        "ExecutionFailed(Provider)" => Some(GenerationExecutionFailureClass::Provider),
        "ExecutionFailed(Tool)" => Some(GenerationExecutionFailureClass::Tool),
        "ExecutionFailed(Hook)" => Some(GenerationExecutionFailureClass::Hook),
        "ExecutionFailed(Workflow)" => Some(GenerationExecutionFailureClass::Workflow),
        "ExecutionFailed(InputResolution)" => {
            Some(GenerationExecutionFailureClass::InputResolution)
        }
        "ExecutionFailed(ClientCapabilityUnavailable)" => {
            Some(GenerationExecutionFailureClass::ClientCapabilityUnavailable)
        }
        "ExecutionFailed(LegacyApprovalRequired)" => {
            Some(GenerationExecutionFailureClass::LegacyApprovalRequired)
        }
        "ExecutionFailed(RuntimeInvariant)" => {
            Some(GenerationExecutionFailureClass::RuntimeInvariant)
        }
        "ExecutionFailed(ExternalEffectAmbiguous)" => {
            Some(GenerationExecutionFailureClass::ExternalEffectAmbiguous)
        }
        "ExecutionFailed(RemoteResourceCleanupAmbiguous)" => {
            Some(GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous)
        }
        _ => None,
    }
}

fn failure_class_for_generation(class: GenerationExecutionFailureClass) -> FailureClass {
    match class {
        GenerationExecutionFailureClass::Provider => FailureClass::Provider,
        GenerationExecutionFailureClass::Tool => FailureClass::Tool,
        GenerationExecutionFailureClass::Hook => FailureClass::Hook,
        GenerationExecutionFailureClass::Workflow => FailureClass::Workflow,
        GenerationExecutionFailureClass::InputResolution => FailureClass::InputResolution,
        GenerationExecutionFailureClass::ClientCapabilityUnavailable => {
            FailureClass::ClientCapabilityUnavailable
        }
        GenerationExecutionFailureClass::LegacyApprovalRequired => {
            FailureClass::LegacyApprovalRequired
        }
        GenerationExecutionFailureClass::RuntimeInvariant => FailureClass::RuntimeInvariant,
        GenerationExecutionFailureClass::ExternalEffectAmbiguous => {
            FailureClass::ExternalEffectAmbiguous
        }
        GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous => {
            FailureClass::RemoteResourceCleanupAmbiguous
        }
    }
}

fn reservation_terminal_fixture(
    reason: ReservationFinalizerReason,
    terminal_reason: NotAdmittedReason,
) -> OperationTerminalFixture {
    OperationTerminalFixture {
        operation: operation_record(),
        selected_cause: OperationFinalizationCause::Reservation(reason),
        suspended_cause: None,
        terminal: OperationTerminal::NotAdmitted {
            reason: terminal_reason,
        },
        expected_settlements: Vec::new(),
        settlement_receipts: Vec::new(),
    }
}

fn suspended_terminal_fixture(
    cause: SuspendedFinalizationCause,
    terminal: OperationTerminal,
) -> OperationTerminalFixture {
    let mut operation = operation_record();
    operation.phase = OperationPhase::Suspended {
        cause: SuspensionCause::Interrupted {
            generation_id: SurfaceGenerationId::new(0),
        },
    };
    let stopped = generation_at(
        &operation,
        0,
        GenerationPhase::Stopped,
        Some(GenerationStopReason::InterruptedResumable),
    );
    operation.generations.push(stopped);
    OperationTerminalFixture {
        operation,
        selected_cause: OperationFinalizationCause::Suspended(cause.clone()),
        suspended_cause: Some(cause),
        terminal,
        expected_settlements: Vec::new(),
        settlement_receipts: Vec::new(),
    }
}

fn operation_terminal_fixtures(
    row: &ManifestOperationTerminalRow,
    seed: u32,
) -> Option<Vec<OperationTerminalFixture>> {
    if row.target.starts_with("ContinueGoal")
        || row.target.starts_with("Suspended")
        || row.target.starts_with("Finalizing")
    {
        return None;
    }

    let current = replayability();
    let fixtures = match row.source.as_str() {
        "Completed(Success)" => {
            let mut fixture = generation_terminal_fixture(
                GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                },
                OperationTerminal::Succeeded {
                    usage: vector_usage(),
                },
                current,
            );
            if row.target == "Terminal(decision.terminal)" {
                fixture.operation.intent.kind = OperationKind::GoalRun {
                    goal_id: SurfaceGoalId::try_new(format!("terminal-goal-{seed}")).unwrap(),
                    goal_run_id: SurfaceGoalRunId::try_new(format!("terminal-run-{seed}")).unwrap(),
                    initial_objective_revision: GoalObjectiveRevision::new(1),
                };
            }
            vec![fixture]
        }
        "Completed(VerificationFailed)" => {
            let message = safe_message("verification failed");
            vec![generation_terminal_fixture(
                GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::VerificationFailed {
                        message: message.clone(),
                    },
                },
                OperationTerminal::Failed {
                    class: FailureClass::Verification,
                    message,
                },
                current,
            )]
        }
        source if budget_for_terminal_source(source, seed).is_some() => {
            let budget = budget_for_terminal_source(source, seed).unwrap();
            vec![generation_terminal_fixture(
                GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::BudgetExhausted {
                        budget: budget.clone(),
                    },
                },
                OperationTerminal::BudgetExhausted { budget },
                current,
            )]
        }
        "Cancelled(UserCancel)" => vec![generation_terminal_fixture(
            GenerationStopReason::Cancelled {
                cause: TerminalizationCause::UserCancel,
            },
            terminalization_terminal(TerminalizationCause::UserCancel),
            current,
        )],
        "Cancelled(GoalPause)" => vec![generation_terminal_fixture(
            GenerationStopReason::Cancelled {
                cause: TerminalizationCause::GoalPause,
            },
            terminalization_terminal(TerminalizationCause::GoalPause),
            current,
        )],
        "Cancelled(HostShutdown)" => vec![generation_terminal_fixture(
            GenerationStopReason::Cancelled {
                cause: TerminalizationCause::HostShutdown,
            },
            terminalization_terminal(TerminalizationCause::HostShutdown),
            current,
        )],
        "Cancelled(ThreadClose)" => vec![generation_terminal_fixture(
            GenerationStopReason::Cancelled {
                cause: TerminalizationCause::ThreadClose,
            },
            terminalization_terminal(TerminalizationCause::ThreadClose),
            current,
        )],
        "InterruptedResumable" => vec![generation_terminal_fixture(
            GenerationStopReason::InterruptedResumable,
            OperationTerminal::AbortedByRuntimeRestart {
                last_generation: SurfaceGenerationId::new(0),
            },
            noncurrent_replayability(),
        )],
        "ProviderSuspended" => vec![generation_terminal_fixture(
            GenerationStopReason::ProviderSuspended,
            OperationTerminal::AbortedByRuntimeRestart {
                last_generation: SurfaceGenerationId::new(0),
            },
            noncurrent_replayability(),
        )],
        "RuntimeRestart" => vec![generation_terminal_fixture(
            GenerationStopReason::RuntimeRestart,
            OperationTerminal::AbortedByRuntimeRestart {
                last_generation: SurfaceGenerationId::new(0),
            },
            current,
        )],
        "ProjectionFailure" => {
            let message = safe_message("projection failed");
            vec![generation_terminal_fixture(
                GenerationStopReason::ProjectionFailure {
                    message: message.clone(),
                },
                OperationTerminal::Failed {
                    class: FailureClass::Persistence,
                    message,
                },
                current,
            )]
        }
        source if execution_failure_class_for_source(source).is_some() => {
            let class = execution_failure_class_for_source(source).unwrap();
            let message = safe_message("execution failed");
            vec![generation_terminal_fixture(
                GenerationStopReason::ExecutionFailed {
                    class,
                    message: message.clone(),
                },
                OperationTerminal::Failed {
                    class: failure_class_for_generation(class),
                    message,
                },
                current,
            )]
        }
        "Panicked" => {
            let message = safe_message("generation panicked");
            vec![generation_terminal_fixture(
                GenerationStopReason::Panicked {
                    message: message.clone(),
                },
                OperationTerminal::Panicked { message },
                current,
            )]
        }
        "NotStarted(Cancelled(UserCancel))" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::Cancelled {
                    cause: TerminalizationCause::UserCancel,
                },
            },
            terminalization_terminal(TerminalizationCause::UserCancel),
            current,
        )],
        "NotStarted(Cancelled(GoalPause))" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::Cancelled {
                    cause: TerminalizationCause::GoalPause,
                },
            },
            terminalization_terminal(TerminalizationCause::GoalPause),
            current,
        )],
        "NotStarted(Cancelled(HostShutdown))" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::Cancelled {
                    cause: TerminalizationCause::HostShutdown,
                },
            },
            terminalization_terminal(TerminalizationCause::HostShutdown),
            current,
        )],
        "NotStarted(Cancelled(ThreadClose))" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::Cancelled {
                    cause: TerminalizationCause::ThreadClose,
                },
            },
            terminalization_terminal(TerminalizationCause::ThreadClose),
            current,
        )],
        "NotStarted(Interrupted)" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::Interrupted,
            },
            OperationTerminal::AbortedByRuntimeRestart {
                last_generation: SurfaceGenerationId::new(0),
            },
            noncurrent_replayability(),
        )],
        "NotStarted(RuntimeRestart)" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::RuntimeRestart,
            },
            OperationTerminal::AbortedByRuntimeRestart {
                last_generation: SurfaceGenerationId::new(0),
            },
            noncurrent_replayability(),
        )],
        "NotStarted(ReservationExpired)" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::ReservationExpired,
            },
            OperationTerminal::NotAdmitted {
                reason: NotAdmittedReason::ReservationExpired,
            },
            current,
        )],
        "NotStarted(AdmissionRejected(ConfigurationConflict))" => {
            vec![generation_terminal_fixture(
                GenerationStopReason::NotStarted {
                    reason: NotStartedReason::AdmissionRejected {
                        reason: AdmissionRejectionReason::ConfigurationConflict,
                    },
                },
                OperationTerminal::NotAdmitted {
                    reason: NotAdmittedReason::ConfigurationConflict,
                },
                current,
            )]
        }
        "NotStarted(AdmissionRejected(PolicyConflict))" => {
            vec![generation_terminal_fixture(
                GenerationStopReason::NotStarted {
                    reason: NotStartedReason::AdmissionRejected {
                        reason: AdmissionRejectionReason::PolicyConflict,
                    },
                },
                OperationTerminal::NotAdmitted {
                    reason: NotAdmittedReason::PolicyConflict,
                },
                current,
            )]
        }
        "NotStarted(StartCommitFailure)" => {
            let message = safe_message("start commit failed");
            vec![generation_terminal_fixture(
                GenerationStopReason::NotStarted {
                    reason: NotStartedReason::StartCommitFailure {
                        message: message.clone(),
                    },
                },
                OperationTerminal::Failed {
                    class: FailureClass::Persistence,
                    message,
                },
                current,
            )]
        }
        "NotStarted(MissingLiveInputCapsule)" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::MissingLiveInputCapsule,
            },
            OperationTerminal::Failed {
                class: FailureClass::RuntimeInvariant,
                message: safe_message(
                    "non-replayable operation input capsule is unavailable before generation start",
                ),
            },
            current,
        )],
        "NotStarted(Shutdown(HostShutdown))" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::Shutdown {
                    reason: SurfaceShutdownReason::HostShutdown,
                },
            },
            OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::HostShutdown,
            },
            current,
        )],
        "NotStarted(Shutdown(ThreadClose))" => vec![generation_terminal_fixture(
            GenerationStopReason::NotStarted {
                reason: NotStartedReason::Shutdown {
                    reason: SurfaceShutdownReason::ThreadClose,
                },
            },
            OperationTerminal::Shutdown {
                reason: SurfaceShutdownReason::ThreadClose,
            },
            current,
        )],
        "ReservationFinalizer(ReservationExpired)" => vec![reservation_terminal_fixture(
            ReservationFinalizerReason::ReservationExpired,
            NotAdmittedReason::ReservationExpired,
        )],
        "ReservationFinalizer(AdmissionRejected(ConfigurationConflict))" => {
            vec![reservation_terminal_fixture(
                ReservationFinalizerReason::AdmissionRejected {
                    reason: AdmissionRejectionReason::ConfigurationConflict,
                },
                NotAdmittedReason::ConfigurationConflict,
            )]
        }
        "ReservationFinalizer(AdmissionRejected(PolicyConflict))" => {
            vec![reservation_terminal_fixture(
                ReservationFinalizerReason::AdmissionRejected {
                    reason: AdmissionRejectionReason::PolicyConflict,
                },
                NotAdmittedReason::PolicyConflict,
            )]
        }
        "ReservationFinalizer(CancelledBeforeAdmission)" => {
            vec![reservation_terminal_fixture(
                ReservationFinalizerReason::CancelledBeforeAdmission,
                NotAdmittedReason::CancelledBeforeAdmission,
            )]
        }
        "ReservationFinalizer(RuntimeRestart)" => vec![reservation_terminal_fixture(
            ReservationFinalizerReason::RuntimeRestart,
            NotAdmittedReason::RuntimeRestart,
        )],
        "ReservationFinalizer(HostShutdown)" => vec![reservation_terminal_fixture(
            ReservationFinalizerReason::HostShutdown,
            NotAdmittedReason::HostShutdown,
        )],
        "ReservationFinalizer(ThreadClose)" => vec![reservation_terminal_fixture(
            ReservationFinalizerReason::ThreadClose,
            NotAdmittedReason::ThreadClose,
        )],
        "OperationJoinSettlement" => {
            let operation = operation_record();
            let finalize_intent_id =
                SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
            let settlement_id =
                SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(seed + 2)).unwrap();
            let receipt = SurfaceSettlementReceipt {
                settlement_id: settlement_id.clone(),
                receipt_digest: digest(143),
            };
            let message = safe_message("operation join failed");
            vec![OperationTerminalFixture {
                selected_cause: OperationFinalizationCause::OperationJoinSettlement(
                    OperationJoinSettlementSource {
                        operation_id: operation.operation_id.clone(),
                        finalize_intent_id,
                        settlement_id: settlement_id.clone(),
                        settlement_receipt_digest: receipt.receipt_digest.clone(),
                        message: message.clone(),
                    },
                ),
                suspended_cause: None,
                terminal: OperationTerminal::JoinFailed { message },
                expected_settlements: vec![settlement_id],
                settlement_receipts: vec![receipt],
                operation,
            }]
        }
        "SuspendedFinalization(ResumeStartCommitFailure)" => {
            let message = safe_message("resume start commit failed");
            vec![suspended_terminal_fixture(
                SuspendedFinalizationCause::ResumeStartCommitFailure {
                    message: message.clone(),
                },
                OperationTerminal::Failed {
                    class: FailureClass::Persistence,
                    message,
                },
            )]
        }
        "SuspendedFinalization(RecoveryAbortNonReplayable)" => {
            vec![suspended_terminal_fixture(
                SuspendedFinalizationCause::RecoveryAbortNonReplayable {
                    last_generation: SurfaceGenerationId::new(0),
                },
                OperationTerminal::AbortedByRuntimeRestart {
                    last_generation: SurfaceGenerationId::new(0),
                },
            )]
        }
        "SuspendedFinalization(Terminalization(cause))" => [
            TerminalizationCause::UserCancel,
            TerminalizationCause::GoalPause,
            TerminalizationCause::HostShutdown,
            TerminalizationCause::ThreadClose,
        ]
        .into_iter()
        .map(|cause| {
            suspended_terminal_fixture(
                SuspendedFinalizationCause::Terminalization(cause),
                terminalization_terminal(cause),
            )
        })
        .collect(),
        unknown => panic!(
            "unrecognized terminal-producing operation_terminal_mapping row: {unknown}|{}|{}",
            row.target, row.condition
        ),
    };
    Some(fixtures)
}

fn mismatched_terminal(terminal: &OperationTerminal) -> OperationTerminal {
    if matches!(terminal, OperationTerminal::Panicked { .. }) {
        OperationTerminal::Cancelled {
            reason: CancelReason::User,
        }
    } else {
        OperationTerminal::Panicked {
            message: safe_message("wrong terminal mapping"),
        }
    }
}

fn push_if_not_rejected(
    failures: &mut Vec<String>,
    label: impl Into<String>,
    result: SurfaceReduceResult,
) {
    if !matches!(result, SurfaceReduceResult::Rejected { .. }) {
        failures.push(label.into());
    }
}

fn operation_transition_signature(row: &ManifestTransitionRow) -> String {
    format!(
        "{}|{}|{}|{}",
        row.source,
        row.target,
        row.event,
        row.invariants.join(",")
    )
}

fn generation_transition_signature(row: &ManifestTransitionRow) -> String {
    operation_transition_signature(row)
}

fn foreground_generation_state(
    mut operation: OperationRecord,
    generation: GenerationRecord,
) -> SurfaceReducerState {
    operation.phase = OperationPhase::Admitted;
    operation.generations = vec![generation];
    operation_state(operation, false)
}

fn transferred_generation_state(
    mut operation: OperationRecord,
    generation: GenerationRecord,
    fence: SurfaceBackgroundFence,
) -> SurfaceReducerState {
    operation.phase = OperationPhase::Admitted;
    operation.generations = vec![generation];
    let mut initial = snapshot();
    initial.operation_history.push(operation);
    initial
        .background_operations
        .push(SurfaceBackgroundOperation {
            operation_id: fence.operation_fence.operation_id.clone(),
            fence,
            task_id: None,
            transferred_at: cursor(0),
            finalizing_degraded: false,
        });
    SurfaceReducerState::new(initial)
}

fn exercise_generation_transition(
    row: &ManifestTransitionRow,
    index: usize,
    failures: &mut Vec<String>,
) {
    let label = generation_transition_signature(row);
    let seed = 31_000 + index as u32 * 100;
    match label.as_str() {
        "Reserved|Started|Operation.GenerationStarted|same_operation_fence,settings_policy_replayability_frozen" =>
        {
            let operation = operation_record();
            let generation = generation_at(&operation, 0, GenerationPhase::Reserved, None);
            let initial = foreground_generation_state(operation.clone(), generation.clone());
            let witness = generation_started_witness(seed, &generation.replayability);
            let started = |fence, witness| OperationPatch::GenerationStarted { fence, witness };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(started(generation.fence.clone(), witness.clone())),
                )],
            );
            assert_vector_applied(
                &label,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );

            let mut wrong_fence = generation.fence.clone();
            wrong_fence.thread_owner_epoch = ThreadOwnerEpoch::new(2);
            let wrong_fence_batch = batch(
                &initial,
                seed + 2,
                vec![(
                    SurfaceScope::Generation {
                        fence: wrong_fence.clone(),
                    },
                    SurfaceEvent::Operation(started(wrong_fence, witness.clone())),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:same_operation_fence"),
                reduce_batch(SurfaceReduceMode::Live, &initial, &wrong_fence_batch),
            );

            let mut wrong_replayability = witness.clone();
            wrong_replayability.durable_replayability_digest = digest(249);
            let wrong_replayability_batch = batch(
                &initial,
                seed + 3,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(started(generation.fence.clone(), wrong_replayability)),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:settings_policy_replayability_frozen"),
                reduce_batch(
                    SurfaceReduceMode::Live,
                    &initial,
                    &wrong_replayability_batch,
                ),
            );

            let stopped_generation = generation_at(
                &operation,
                0,
                GenerationPhase::Stopped,
                Some(GenerationStopReason::NotStarted {
                    reason: NotStartedReason::Interrupted,
                }),
            );
            let omitted_state = foreground_generation_state(operation, stopped_generation.clone());
            let omitted = batch(
                &omitted_state,
                seed + 4,
                vec![(
                    generation_scope(&stopped_generation),
                    SurfaceEvent::Operation(started(stopped_generation.fence, witness)),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:omitted_edge"),
                reduce_batch(SurfaceReduceMode::Live, &omitted_state, &omitted),
            );
        }
        "Reserved|Stopped(NotStarted)|Operation.GenerationStopped|not_started_reason" => {
            let operation = operation_record();
            let generation = generation_at(&operation, 0, GenerationPhase::Reserved, None);
            let initial = foreground_generation_state(operation.clone(), generation.clone());
            let stopped = |fence, reason| OperationPatch::GenerationStopped {
                fence,
                reason,
                usage_delta: vector_usage(),
            };
            let not_started = GenerationStopReason::NotStarted {
                reason: NotStartedReason::Interrupted,
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(stopped(generation.fence.clone(), not_started.clone())),
                )],
            );
            assert_vector_applied(
                &label,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );

            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(stopped(
                        generation.fence.clone(),
                        GenerationStopReason::Completed {
                            status: GenerationCompletionStatus::Success,
                        },
                    )),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:not_started_reason"),
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );

            let started_generation = generation_at(&operation, 0, GenerationPhase::Started, None);
            let omitted_state = foreground_generation_state(operation, started_generation.clone());
            let omitted = batch(
                &omitted_state,
                seed + 2,
                vec![(
                    generation_scope(&started_generation),
                    SurfaceEvent::Operation(stopped(started_generation.fence, not_started)),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:omitted_edge"),
                reduce_batch(SurfaceReduceMode::Live, &omitted_state, &omitted),
            );
        }
        "Started|Stopped|Operation.GenerationStopped|stop_reason" => {
            let operation = operation_record();
            let generation = generation_at(&operation, 0, GenerationPhase::Started, None);
            let initial = foreground_generation_state(operation.clone(), generation.clone());
            let stopped = |fence, reason| OperationPatch::GenerationStopped {
                fence,
                reason,
                usage_delta: vector_usage(),
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(stopped(
                        generation.fence.clone(),
                        GenerationStopReason::Completed {
                            status: GenerationCompletionStatus::Success,
                        },
                    )),
                )],
            );
            assert_vector_applied(
                &label,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );

            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(stopped(
                        generation.fence.clone(),
                        GenerationStopReason::NotStarted {
                            reason: NotStartedReason::Interrupted,
                        },
                    )),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:stop_reason"),
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );

            let reserved_generation = generation_at(&operation, 0, GenerationPhase::Reserved, None);
            let omitted_state = foreground_generation_state(operation, reserved_generation.clone());
            let omitted = batch(
                &omitted_state,
                seed + 2,
                vec![(
                    generation_scope(&reserved_generation),
                    SurfaceEvent::Operation(stopped(
                        reserved_generation.fence,
                        GenerationStopReason::Completed {
                            status: GenerationCompletionStatus::Success,
                        },
                    )),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:omitted_edge"),
                reduce_batch(SurfaceReduceMode::Live, &omitted_state, &omitted),
            );
        }
        "Started|Transferred|Operation.GenerationTransferred|background_fence_matches" => {
            let operation = operation_record();
            let generation = generation_at(&operation, 0, GenerationPhase::Started, None);
            let initial = foreground_generation_state(operation.clone(), generation.clone());
            let transferred = |background_fence| OperationPatch::GenerationTransferred {
                fence: generation.fence.clone(),
                background_fence,
                task_id: None,
            };
            let valid_background = background_fence(&generation.fence, 81);
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(transferred(valid_background)),
                )],
            );
            assert_vector_applied(
                &label,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );

            let mut wrong_operation_fence = generation.fence.clone();
            wrong_operation_fence.generation_id = SurfaceGenerationId::new(1);
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(transferred(background_fence(
                        &wrong_operation_fence,
                        81,
                    ))),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:background_fence_matches"),
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );

            let reserved_generation = generation_at(&operation, 0, GenerationPhase::Reserved, None);
            let omitted_state = foreground_generation_state(operation, reserved_generation.clone());
            let omitted = batch(
                &omitted_state,
                seed + 2,
                vec![(
                    generation_scope(&reserved_generation),
                    SurfaceEvent::Operation(OperationPatch::GenerationTransferred {
                        fence: reserved_generation.fence.clone(),
                        background_fence: background_fence(&reserved_generation.fence, 81),
                        task_id: None,
                    }),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:omitted_edge"),
                reduce_batch(SurfaceReduceMode::Live, &omitted_state, &omitted),
            );
        }
        "Transferred|Stopped|Operation.GenerationStopped|background_owner_matches" => {
            let operation = operation_record();
            let generation = generation_at(&operation, 0, GenerationPhase::Transferred, None);
            let owner = background_fence(&generation.fence, 91);
            let initial =
                transferred_generation_state(operation.clone(), generation.clone(), owner.clone());
            let stopped = OperationPatch::GenerationStopped {
                fence: generation.fence.clone(),
                reason: GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                },
                usage_delta: vector_usage(),
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    SurfaceScope::Background {
                        fence: owner.clone(),
                    },
                    SurfaceEvent::Operation(stopped.clone()),
                )],
            );
            assert_vector_applied(
                &label,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );

            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    SurfaceScope::Background {
                        fence: background_fence(&generation.fence, 92),
                    },
                    SurfaceEvent::Operation(stopped.clone()),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:background_owner_matches"),
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );

            let reserved_generation = generation_at(&operation, 0, GenerationPhase::Reserved, None);
            let omitted_state =
                transferred_generation_state(operation, reserved_generation.clone(), owner.clone());
            let omitted = batch(
                &omitted_state,
                seed + 2,
                vec![(
                    SurfaceScope::Background { fence: owner },
                    SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                        fence: reserved_generation.fence,
                        reason: GenerationStopReason::Completed {
                            status: GenerationCompletionStatus::Success,
                        },
                        usage_delta: vector_usage(),
                    }),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:omitted_edge"),
                reduce_batch(SurfaceReduceMode::Live, &omitted_state, &omitted),
            );
        }
        unknown => panic!("unrecognized generation transition manifest row: {unknown}"),
    }
}

fn exercise_operation_transition(
    row: &ManifestTransitionRow,
    index: usize,
    failures: &mut Vec<String>,
) {
    let label = operation_transition_signature(row);
    let seed = 21_000 + index as u32 * 100;
    match label.as_str() {
        "Requested|Admitted|Operation.Admitted|first_generation_reserved" => {
            let operation = operation_record();
            let first_generation = generation(&operation);
            let initial = operation_state(operation.clone(), true);
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Admitted {
                        operation_id: operation.operation_id.clone(),
                        logical_turn_id: first_generation.logical_turn_id.clone(),
                        input: AdmittedInput::NotApplicable,
                        first_generation: first_generation.clone(),
                    }),
                )],
            );
            assert_operation_vector_applied(
                &label,
                failures,
                &initial,
                &positive,
                &operation.operation_id,
            );

            let mut invalid_generation = first_generation;
            invalid_generation.phase = GenerationPhase::Started;
            invalid_generation.started_witness = Some(generation_started_witness(
                seed + 1,
                &invalid_generation.replayability,
            ));
            let invalid = batch(
                &initial,
                seed + 2,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Admitted {
                        operation_id: operation.operation_id.clone(),
                        logical_turn_id: invalid_generation.logical_turn_id.clone(),
                        input: AdmittedInput::NotApplicable,
                        first_generation: invalid_generation,
                    }),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:first_generation_reserved"),
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "Requested|Terminal(NotAdmitted)|Operation.Terminal|no_generation_admitted" => {
            let operation = operation_record();
            let initial = operation_state(operation.clone(), true);
            let finalize_intent_id =
                SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
            let terminal_commit_id = recorded_commit_id(seed);
            let record = OperationTerminalRecord {
                operation_id: operation.operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal: OperationTerminal::NotAdmitted {
                    reason: NotAdmittedReason::ReservationExpired,
                },
                usage: usage(),
                source_diagnostic_digest: None,
                settlement_receipts: Vec::new(),
                completion_proof: SurfaceOperationCompletionProof::unverified(
                    "test terminal has no verifier proof",
                ),
                committed_at: UnixMillis::new(5),
            };
            let events = vec![
                (
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                        operation_id: operation.operation_id.clone(),
                        finalize_intent_id,
                        terminal_commit_id,
                        selected_cause: OperationFinalizationCause::Reservation(
                            ReservationFinalizerReason::ReservationExpired,
                        ),
                        suspended_cause: None,
                        expected_settlements: Vec::new(),
                    }),
                ),
                (
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal { record }),
                ),
            ];
            let positive = batch(&initial, seed, events.clone());
            assert_operation_vector_applied(
                &label,
                failures,
                &initial,
                &positive,
                &operation.operation_id,
            );

            let mut invalid_operation = operation.clone();
            invalid_operation.generations.push(generation_at(
                &invalid_operation,
                0,
                GenerationPhase::Started,
                None,
            ));
            let invalid_state = operation_state(invalid_operation, true);
            let invalid = batch(&invalid_state, seed + 2, events);
            push_if_not_rejected(
                failures,
                format!("{label}:no_generation_admitted"),
                reduce_batch(SurfaceReduceMode::Live, &invalid_state, &invalid),
            );
        }
        "Admitted|Suspended|Operation.SuspendedOrRecoveryRequired|exact_stopped_generation" => {
            let mut operation = operation_record();
            operation.phase = OperationPhase::Admitted;
            operation.generations.push(generation_at(
                &operation,
                0,
                GenerationPhase::Stopped,
                Some(GenerationStopReason::InterruptedResumable),
            ));
            operation.pending_control = Some(PendingControlIntent::Interrupt {
                generation_fence: operation.generations[0].fence.clone(),
            });
            let initial = operation_state(operation.clone(), false);
            let cause = SuspensionCause::Interrupted {
                generation_id: SurfaceGenerationId::new(0),
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Suspended {
                        operation_id: operation.operation_id.clone(),
                        cause: cause.clone(),
                    }),
                )],
            );
            assert_operation_vector_applied(
                &label,
                failures,
                &initial,
                &positive,
                &operation.operation_id,
            );
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Suspended {
                        operation_id: operation.operation_id.clone(),
                        cause: SuspensionCause::Interrupted {
                            generation_id: SurfaceGenerationId::new(1),
                        },
                    }),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:exact_stopped_generation"),
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "Admitted|Finalizing|Operation.FinalizationStarted|all_generations_stopped_or_not_started" =>
        {
            let mut operation = operation_record();
            operation.phase = OperationPhase::Admitted;
            operation.generations.push(generation_at(
                &operation,
                0,
                GenerationPhase::Stopped,
                Some(GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                }),
            ));
            let initial = operation_state(operation.clone(), false);
            let patch = |terminal_commit_id| OperationPatch::FinalizationStarted {
                operation_id: operation.operation_id.clone(),
                finalize_intent_id: SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(
                    seed + 1,
                ))
                .unwrap(),
                terminal_commit_id,
                selected_cause: OperationFinalizationCause::GenerationStop(
                    GenerationStopReason::Completed {
                        status: GenerationCompletionStatus::Success,
                    },
                ),
                suspended_cause: None,
                expected_settlements: Vec::new(),
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(patch(recorded_commit_id(seed))),
                )],
            );
            assert_operation_vector_applied(
                &label,
                failures,
                &initial,
                &positive,
                &operation.operation_id,
            );

            let mut invalid_operation = operation.clone();
            invalid_operation.generations[0].phase = GenerationPhase::Started;
            invalid_operation.generations[0].stop_reason = None;
            let invalid_state = operation_state(invalid_operation, false);
            let invalid = batch(
                &invalid_state,
                seed + 2,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(patch(recorded_commit_id(seed + 2))),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:all_generations_stopped_or_not_started"),
                reduce_batch(SurfaceReduceMode::Live, &invalid_state, &invalid),
            );
        }
        "Suspended|Admitted(GenerationStarted)|Operation.GenerationStarted|matching_ResumeStarting,exact_replacement_fence" =>
        {
            let mut operation = operation_record();
            let previous = generation_at(
                &operation,
                0,
                GenerationPhase::Stopped,
                Some(GenerationStopReason::InterruptedResumable),
            );
            let replacement = generation_at(&operation, 1, GenerationPhase::Reserved, None);
            operation.phase = OperationPhase::Suspended {
                cause: SuspensionCause::Interrupted {
                    generation_id: previous.fence.generation_id,
                },
            };
            operation.generations = vec![previous.clone(), replacement.clone()];
            operation.pending_control = Some(PendingControlIntent::ResumeStarting {
                generation_fence: replacement.fence.clone(),
            });
            let initial = operation_state(operation.clone(), false);
            let start = |fence, event_seed| OperationPatch::GenerationStarted {
                fence,
                witness: generation_started_witness(event_seed, &replacement.replayability),
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&replacement),
                    SurfaceEvent::Operation(start(replacement.fence.clone(), seed)),
                )],
            );
            assert_operation_vector_applied(
                &label,
                failures,
                &initial,
                &positive,
                &operation.operation_id,
            );

            let mut wrong_intent = operation.clone();
            wrong_intent.pending_control = Some(PendingControlIntent::ResumeStarting {
                generation_fence: previous.fence.clone(),
            });
            let wrong_intent_state = operation_state(wrong_intent, false);
            let invalid_intent = batch(
                &wrong_intent_state,
                seed + 2,
                vec![(
                    generation_scope(&replacement),
                    SurfaceEvent::Operation(start(replacement.fence.clone(), seed + 2)),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:matching_ResumeStarting"),
                reduce_batch(
                    SurfaceReduceMode::Live,
                    &wrong_intent_state,
                    &invalid_intent,
                ),
            );

            let mut absent_fence = replacement.fence.clone();
            absent_fence.generation_id = SurfaceGenerationId::new(2);
            let invalid_fence = batch(
                &initial,
                seed + 3,
                vec![(
                    SurfaceScope::Generation {
                        fence: absent_fence.clone(),
                    },
                    SurfaceEvent::Operation(start(absent_fence, seed + 3)),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:exact_replacement_fence"),
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid_fence),
            );
        }
        "Suspended|Suspended(SuspensionRebasedAfterUnstartedResume)|Operation.SuspensionRebasedAfterUnstartedResume|replacement_stopped_NotStarted_Interrupted_or_RuntimeRestart,matching_ResumeStarting" =>
        {
            let mut operation = operation_record();
            let previous = generation_at(
                &operation,
                0,
                GenerationPhase::Stopped,
                Some(GenerationStopReason::InterruptedResumable),
            );
            let replacement = generation_at(
                &operation,
                1,
                GenerationPhase::Stopped,
                Some(GenerationStopReason::NotStarted {
                    reason: NotStartedReason::Interrupted,
                }),
            );
            let previous_cause = SuspensionCause::Interrupted {
                generation_id: previous.fence.generation_id,
            };
            operation.phase = OperationPhase::Suspended {
                cause: previous_cause.clone(),
            };
            operation.generations = vec![previous, replacement.clone()];
            operation.pending_control = Some(PendingControlIntent::ResumeStarting {
                generation_fence: replacement.fence.clone(),
            });
            let initial = operation_state(operation.clone(), false);
            let rebase = OperationPatch::SuspensionRebasedAfterUnstartedResume {
                operation_id: operation.operation_id.clone(),
                previous_cause: previous_cause.clone(),
                replacement_fence: replacement.fence.clone(),
                rebased_cause: SuspensionCause::Interrupted {
                    generation_id: replacement.fence.generation_id,
                },
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(rebase.clone()),
                )],
            );
            assert_operation_vector_applied(
                &label,
                failures,
                &initial,
                &positive,
                &operation.operation_id,
            );

            let mut wrong_stop = operation.clone();
            wrong_stop.generations[1].stop_reason = Some(GenerationStopReason::NotStarted {
                reason: NotStartedReason::Cancelled {
                    cause: TerminalizationCause::UserCancel,
                },
            });
            let wrong_stop_state = operation_state(wrong_stop, false);
            let wrong_stop_batch = batch(
                &wrong_stop_state,
                seed + 1,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(rebase.clone()),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:replacement_stopped_NotStarted_Interrupted_or_RuntimeRestart"),
                reduce_batch(
                    SurfaceReduceMode::Live,
                    &wrong_stop_state,
                    &wrong_stop_batch,
                ),
            );

            let mut missing_intent = operation.clone();
            missing_intent.pending_control = None;
            let missing_intent_state = operation_state(missing_intent, false);
            let missing_intent_batch = batch(
                &missing_intent_state,
                seed + 2,
                vec![(operation_scope(&operation), SurfaceEvent::Operation(rebase))],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:matching_ResumeStarting"),
                reduce_batch(
                    SurfaceReduceMode::Live,
                    &missing_intent_state,
                    &missing_intent_batch,
                ),
            );
        }
        "Suspended|Finalizing(SuspendedFinalizationCause)|Operation.FinalizationStarted|exact_suspended_cause,immutable_selected_cause" =>
        {
            let mut operation = operation_record();
            let stopped = generation_at(
                &operation,
                0,
                GenerationPhase::Stopped,
                Some(GenerationStopReason::InterruptedResumable),
            );
            operation.phase = OperationPhase::Suspended {
                cause: SuspensionCause::Interrupted {
                    generation_id: stopped.fence.generation_id,
                },
            };
            operation.generations.push(stopped);
            let initial = operation_state(operation.clone(), false);
            let selected =
                SuspendedFinalizationCause::Terminalization(TerminalizationCause::UserCancel);
            let finalization =
                |selected_cause, suspended_cause, event_seed| OperationPatch::FinalizationStarted {
                    operation_id: operation.operation_id.clone(),
                    finalize_intent_id: SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(
                        event_seed + 1,
                    ))
                    .unwrap(),
                    terminal_commit_id: recorded_commit_id(event_seed),
                    selected_cause,
                    suspended_cause,
                    expected_settlements: Vec::new(),
                };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(finalization(
                        OperationFinalizationCause::Suspended(selected.clone()),
                        Some(selected.clone()),
                        seed,
                    )),
                )],
            );
            assert_operation_vector_applied(
                &label,
                failures,
                &initial,
                &positive,
                &operation.operation_id,
            );

            let wrong_suspended =
                SuspendedFinalizationCause::Terminalization(TerminalizationCause::GoalPause);
            for (invariant, patch) in [
                (
                    "exact_suspended_cause",
                    finalization(
                        OperationFinalizationCause::Suspended(selected.clone()),
                        Some(wrong_suspended.clone()),
                        seed + 2,
                    ),
                ),
                (
                    "immutable_selected_cause",
                    finalization(
                        OperationFinalizationCause::Suspended(wrong_suspended),
                        Some(selected.clone()),
                        seed + 3,
                    ),
                ),
            ] {
                let invalid = batch(
                    &initial,
                    seed + 10 + invariant.len() as u32,
                    vec![(operation_scope(&operation), SurfaceEvent::Operation(patch))],
                );
                push_if_not_rejected(
                    failures,
                    format!("{label}:{invariant}"),
                    reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
                );
            }
        }
        "Finalizing|Terminal|Operation.Terminal|all_settlements_proved" => {
            let mut operation = operation_record();
            let settlement_id =
                SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
            let receipt = SurfaceSettlementReceipt {
                settlement_id: settlement_id.clone(),
                receipt_digest: digest(41),
            };
            operation.phase = OperationPhase::Finalizing {
                finalize_intent_id: SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(
                    seed + 2,
                ))
                .unwrap(),
            };
            operation.finalization = Some(finalization_record(
                &operation,
                seed,
                OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                }),
                None,
                vec![settlement_id],
                vec![receipt.clone()],
            ));
            operation.phase = OperationPhase::Finalizing {
                finalize_intent_id: operation
                    .finalization
                    .as_ref()
                    .unwrap()
                    .finalize_intent_id
                    .clone(),
            };
            let initial = operation_state(operation.clone(), false);
            let record = terminal_record_for(
                &operation,
                OperationTerminal::Succeeded {
                    usage: vector_usage(),
                },
                vector_usage(),
                vec![receipt],
            );
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal { record }),
                )],
            );
            assert_operation_vector_applied(
                &label,
                failures,
                &initial,
                &positive,
                &operation.operation_id,
            );

            let mut unsettled = operation.clone();
            unsettled.finalization.as_mut().unwrap().settled.clear();
            let unsettled_state = operation_state(unsettled.clone(), false);
            let invalid_record = terminal_record_for(
                &unsettled,
                OperationTerminal::Succeeded {
                    usage: vector_usage(),
                },
                vector_usage(),
                Vec::new(),
            );
            let invalid = batch(
                &unsettled_state,
                seed + 3,
                vec![(
                    operation_scope(&unsettled),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: invalid_record,
                    }),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:all_settlements_proved"),
                reduce_batch(SurfaceReduceMode::Live, &unsettled_state, &invalid),
            );
        }
        "Finalizing|FinalizingDegraded|Operation.FinalizationDegraded|missing_settlement_or_terminal_projection_recorded" =>
        {
            let mut operation = operation_record();
            let settlement_id =
                SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
            operation.finalization = Some(finalization_record(
                &operation,
                seed + 10,
                OperationFinalizationCause::GenerationStop(
                    GenerationStopReason::ProjectionFailure {
                        message: safe_message("projection failed"),
                    },
                ),
                None,
                vec![settlement_id.clone()],
                Vec::new(),
            ));
            operation.phase = OperationPhase::Finalizing {
                finalize_intent_id: operation
                    .finalization
                    .as_ref()
                    .unwrap()
                    .finalize_intent_id
                    .clone(),
            };
            let initial = operation_state(operation.clone(), false);
            let finalization = operation.finalization.as_ref().unwrap();
            let positive_cause = FinalizationDegradedCause::MissingFinalization {
                terminal_commit_id: finalization.terminal_commit_id.clone(),
                missing_settlements: NonEmptyVec::try_new(vec![settlement_id.clone()]).unwrap(),
                missing_set_digest: digest(42),
            };
            let degraded = |cause| OperationPatch::FinalizationDegraded {
                operation_id: operation.operation_id.clone(),
                finalize_intent_id: finalization.finalize_intent_id.clone(),
                cause,
                last_error: DisplayText::new("degraded"),
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(degraded(positive_cause)),
                )],
            );
            assert_operation_vector_applied(
                &label,
                failures,
                &initial,
                &positive,
                &operation.operation_id,
            );

            let wrong_settlement =
                SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(seed + 2)).unwrap();
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(degraded(
                        FinalizationDegradedCause::MissingFinalization {
                            terminal_commit_id: finalization.terminal_commit_id.clone(),
                            missing_settlements: NonEmptyVec::try_new(vec![wrong_settlement])
                                .unwrap(),
                            missing_set_digest: digest(43),
                        },
                    )),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:missing_settlement_or_terminal_projection_recorded"),
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "FinalizingDegraded|Terminal(RetryFinalization)|RetryFinalization|same_finalize_intent_and_terminal_commit_id" =>
        {
            let terminal_seed = seed + 3;
            let mut operation = operation_record();
            let settlement_id =
                SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
            let receipt = SurfaceSettlementReceipt {
                settlement_id: settlement_id.clone(),
                receipt_digest: digest(51),
            };
            operation.finalization = Some(finalization_record(
                &operation,
                terminal_seed,
                OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                }),
                None,
                vec![settlement_id.clone()],
                Vec::new(),
            ));
            operation.phase = OperationPhase::Finalizing {
                finalize_intent_id: operation
                    .finalization
                    .as_ref()
                    .unwrap()
                    .finalize_intent_id
                    .clone(),
            };
            let finalizing_state = operation_state(operation.clone(), false);
            let finalization = operation.finalization.as_ref().unwrap();
            let degraded = batch(
                &finalizing_state,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::FinalizationDegraded {
                        operation_id: operation.operation_id.clone(),
                        finalize_intent_id: finalization.finalize_intent_id.clone(),
                        cause: FinalizationDegradedCause::MissingFinalization {
                            terminal_commit_id: finalization.terminal_commit_id.clone(),
                            missing_settlements: NonEmptyVec::try_new(vec![settlement_id.clone()])
                                .unwrap(),
                            missing_set_digest: digest(52),
                        },
                        last_error: DisplayText::new("settlement missing"),
                    }),
                )],
            );
            let degraded_state = assert_vector_applied(
                &format!("{label}:proof:MissingFinalization"),
                reduce_batch(SurfaceReduceMode::Live, &finalizing_state, &degraded),
            );
            let settlement = batch(
                &degraded_state,
                seed + 1,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::FinalizationSettlementRecorded {
                        operation_id: operation.operation_id.clone(),
                        finalize_intent_id: finalization.finalize_intent_id.clone(),
                        receipt: receipt.clone(),
                    }),
                )],
            );
            let ready_state = assert_vector_applied(
                &format!("{label}:proof:settlement"),
                reduce_batch(SurfaceReduceMode::Live, &degraded_state, &settlement),
            );
            let record = terminal_record_for(
                &operation,
                OperationTerminal::Succeeded {
                    usage: vector_usage(),
                },
                vector_usage(),
                vec![receipt],
            );
            let positive = batch(
                &ready_state,
                terminal_seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: record.clone(),
                    }),
                )],
            );
            assert_operation_vector_applied(
                &label,
                failures,
                &ready_state,
                &positive,
                &operation.operation_id,
            );

            let mut wrong_intent = record.clone();
            wrong_intent.finalize_intent_id =
                SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(seed + 20)).unwrap();
            let invalid_intent = batch(
                &ready_state,
                terminal_seed + 1,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: wrong_intent,
                    }),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:same_finalize_intent_and_terminal_commit_id:finalize_intent_id"),
                reduce_batch(SurfaceReduceMode::Live, &ready_state, &invalid_intent),
            );

            let invalid_commit = batch(
                &ready_state,
                terminal_seed + 99,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal { record }),
                )],
            );
            push_if_not_rejected(
                failures,
                format!("{label}:same_finalize_intent_and_terminal_commit_id:terminal_commit_id"),
                reduce_batch(SurfaceReduceMode::Live, &ready_state, &invalid_commit),
            );
        }
        "FinalizingDegraded|Terminal(RetryProjectionTerminal)|RetryProjection|same_durable_terminal_event_and_terminal_commit_id" =>
        {
            let terminal_seed = seed + 2;
            let terminal_event_id = SurfaceEventId::try_from_bytes(uuid_v7_bytes(
                terminal_seed.wrapping_mul(2_000).wrapping_add(1),
            ))
            .unwrap();
            let mut operation = operation_record();
            operation.finalization = Some(finalization_record(
                &operation,
                terminal_seed,
                OperationFinalizationCause::GenerationStop(
                    GenerationStopReason::ProjectionFailure {
                        message: safe_message("projection failed"),
                    },
                ),
                None,
                Vec::new(),
                Vec::new(),
            ));
            operation.phase = OperationPhase::Finalizing {
                finalize_intent_id: operation
                    .finalization
                    .as_ref()
                    .unwrap()
                    .finalize_intent_id
                    .clone(),
            };
            let finalizing_state = operation_state(operation.clone(), false);
            let finalization = operation.finalization.as_ref().unwrap();
            let degradation = |terminal_event_id| OperationPatch::FinalizationDegraded {
                operation_id: operation.operation_id.clone(),
                finalize_intent_id: finalization.finalize_intent_id.clone(),
                cause: FinalizationDegradedCause::TerminalProjectionPending {
                    terminal_commit_id: finalization.terminal_commit_id.clone(),
                    terminal_event_id,
                    durable_revision: DurableRevision::try_new(1).unwrap(),
                    terminal_digest: digest(61),
                },
                last_error: DisplayText::new("terminal projection pending"),
            };
            let degraded = batch(
                &finalizing_state,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(degradation(terminal_event_id.clone())),
                )],
            );
            let degraded_state = assert_vector_applied(
                &format!("{label}:proof:TerminalProjectionPending"),
                reduce_batch(SurfaceReduceMode::Live, &finalizing_state, &degraded),
            );
            let record = terminal_record_for(
                &operation,
                OperationTerminal::Failed {
                    class: FailureClass::Persistence,
                    message: safe_message("projection failed"),
                },
                vector_usage(),
                Vec::new(),
            );
            let positive = batch(
                &degraded_state,
                terminal_seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: record.clone(),
                    }),
                )],
            );
            assert_eq!(positive.events.as_slice()[0].event_id, terminal_event_id);
            assert_operation_vector_applied(
                &label,
                failures,
                &degraded_state,
                &positive,
                &operation.operation_id,
            );

            let wrong_event_id =
                SurfaceEventId::try_from_bytes(uuid_v7_bytes(terminal_seed + 99)).unwrap();
            let wrong_degraded = batch(
                &finalizing_state,
                seed + 1,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(degradation(wrong_event_id)),
                )],
            );
            if let SurfaceReduceResult::Applied {
                state: wrong_degraded_state,
            } = reduce_batch(SurfaceReduceMode::Live, &finalizing_state, &wrong_degraded)
            {
                let invalid_event = batch(
                    &wrong_degraded_state,
                    terminal_seed,
                    vec![(
                        operation_scope(&operation),
                        SurfaceEvent::Operation(OperationPatch::Terminal {
                            record: record.clone(),
                        }),
                    )],
                );
                push_if_not_rejected(
                    failures,
                    format!(
                        "{label}:same_durable_terminal_event_and_terminal_commit_id:terminal_event_id"
                    ),
                    reduce_batch(
                        SurfaceReduceMode::Live,
                        &wrong_degraded_state,
                        &invalid_event,
                    ),
                );
            }

            let invalid_commit = batch(
                &degraded_state,
                terminal_seed + 99,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal { record }),
                )],
            );
            push_if_not_rejected(
                failures,
                format!(
                    "{label}:same_durable_terminal_event_and_terminal_commit_id:terminal_commit_id"
                ),
                reduce_batch(SurfaceReduceMode::Live, &degraded_state, &invalid_commit),
            );
        }
        unknown => panic!("unrecognized operation transition manifest row: {unknown}"),
    }
}

#[test]
fn operation_transitions_execute_every_manifest_row_once() {
    let rows = manifest_transition_rows("operation_transitions");
    assert_eq!(
        rows.len(),
        11,
        "operation transition manifest row count drifted"
    );

    let mut consumed = BTreeSet::new();
    let mut failures = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let signature = operation_transition_signature(row);
        assert!(
            consumed.insert(signature.clone()),
            "duplicate operation transition manifest row: {signature}"
        );
        exercise_operation_transition(row, index, &mut failures);
    }

    assert_eq!(consumed.len(), rows.len());
    assert!(
        failures.is_empty(),
        "operation transition rejection mutations unexpectedly applied ({}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn generation_transitions_execute_every_manifest_row_once() {
    let rows = manifest_transition_rows("generation_transitions");
    assert_eq!(
        rows.len(),
        5,
        "generation transition manifest row count drifted"
    );

    let mut consumed = BTreeSet::new();
    let mut failures = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let signature = generation_transition_signature(row);
        assert!(
            consumed.insert(signature.clone()),
            "duplicate generation transition manifest row: {signature}"
        );
        exercise_generation_transition(row, index, &mut failures);
    }

    assert_eq!(consumed.len(), rows.len());
    assert!(
        failures.is_empty(),
        "generation transition rejection mutations unexpectedly applied ({}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn stopped_admitted_operation() -> (OperationRecord, GenerationRecord) {
    let mut operation = operation_record();
    operation.phase = OperationPhase::Admitted;
    let generation = generation_at(
        &operation,
        0,
        GenerationPhase::Stopped,
        Some(GenerationStopReason::Completed {
            status: GenerationCompletionStatus::Success,
        }),
    );
    operation.generations.push(generation.clone());
    (operation, generation)
}

fn finalizing_invariant_state(
    operation: &OperationRecord,
    finalization_seed: u32,
    terminal_seed: u32,
    selected_cause: OperationFinalizationCause,
) -> (SurfaceReducerState, SurfaceFinalizeIntentId) {
    let initial = operation_state(operation.clone(), false);
    let finalize_intent_id =
        SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(finalization_seed + 1)).unwrap();
    let finalization = batch(
        &initial,
        finalization_seed,
        vec![(
            operation_scope(operation),
            SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                operation_id: operation.operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: recorded_commit_id(terminal_seed),
                selected_cause,
                suspended_cause: None,
                expected_settlements: Vec::new(),
            }),
        )],
    );
    (
        assert_vector_applied(
            "operation_generation_invariant:finalization_setup",
            reduce_batch(SurfaceReduceMode::Live, &initial, &finalization),
        ),
        finalize_intent_id,
    )
}

fn invariant_terminal_record(
    operation_id: SurfaceOperationId,
    finalize_intent_id: SurfaceFinalizeIntentId,
    terminal: OperationTerminal,
    usage: UsageTotals,
) -> OperationTerminalRecord {
    OperationTerminalRecord {
        operation_id,
        finalize_intent_id,
        terminal,
        usage,
        source_diagnostic_digest: None,
        settlement_receipts: Vec::new(),
        completion_proof: SurfaceOperationCompletionProof::unverified(
            "test terminal has no verifier proof",
        ),
        committed_at: UnixMillis::new(9),
    }
}

#[test]
fn terminal_rejects_completion_proof_receipt_not_backed_by_operation_tools() {
    let (operation, _) = stopped_admitted_operation();
    let terminal_seed = 16_900;
    let (finalizing, finalize_intent_id) = finalizing_invariant_state(
        &operation,
        terminal_seed - 1,
        terminal_seed,
        OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
            status: GenerationCompletionStatus::Success,
        }),
    );
    let mut record = invariant_terminal_record(
        operation.operation_id.clone(),
        finalize_intent_id,
        OperationTerminal::Succeeded {
            usage: vector_usage(),
        },
        vector_usage(),
    );
    record
        .completion_proof
        .tool_receipts
        .push(SurfaceToolCompletionReceipt {
            tool_call_id: SurfaceToolCallId::try_new("forged-receipt").unwrap(),
            terminal: SurfaceToolTerminal {
                kind: SurfaceToolResultKind::Success,
                source: ToolTerminalSource::Observed,
                invocation_started: ToolInvocationStarted::Yes,
            },
            result_digest: digest(91),
            file_change_digest: None,
        });
    let terminal = batch(
        &finalizing,
        terminal_seed,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::Terminal { record }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &finalizing, &terminal),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn terminal_record_without_completion_proof_defaults_to_unverified() {
    let record = invariant_terminal_record(
        SurfaceOperationId::try_from_bytes(uuid_v7_bytes(16_910)).unwrap(),
        SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(16_911)).unwrap(),
        OperationTerminal::Succeeded {
            usage: vector_usage(),
        },
        vector_usage(),
    );
    let mut serialized = serde_json::to_value(record).unwrap();
    serialized
        .as_object_mut()
        .unwrap()
        .remove("completion_proof");

    let restored: OperationTerminalRecord = serde_json::from_value(serialized).unwrap();
    assert_eq!(
        restored.completion_proof.verification,
        SurfaceCompletionVerification::Unverified
    );
}

fn goal_generation_identity(
    generation: &GenerationRecord,
    goal_id: SurfaceGoalId,
    goal_run_id: SurfaceGoalRunId,
    outer_turn_count: u32,
) -> SurfaceGoalGenerationIdentity {
    let canonical_input_item_id = match &generation.input {
        GenerationInputState::Pending { input_item_id, .. }
        | GenerationInputState::Resolved { input_item_id, .. }
        | GenerationInputState::Failed { input_item_id, .. } => input_item_id.clone(),
        GenerationInputState::NotApplicable => {
            panic!("goal generation invariant requires a canonical input item")
        }
    };
    SurfaceGoalGenerationIdentity {
        goal_id,
        goal_run_id,
        operation_fence: generation.fence.clone(),
        goal_outer_turn_id: SurfaceGoalOuterTurnId::try_new(format!(
            "manifest-outer-{}",
            generation.fence.generation_id.get()
        ))
        .unwrap(),
        logical_turn_id: generation.logical_turn_id.clone(),
        canonical_input_item_id,
        outer_turn_origin: if generation.fence.generation_id.get() == 0 {
            GoalOuterTurnOrigin::User
        } else {
            GoalOuterTurnOrigin::Continuation
        },
        attempt: generation.attempt,
        predecessor_fence: generation.predecessor.clone(),
        objective_revision: GoalObjectiveRevision::new(1),
        outer_turn_count,
    }
}

fn invariant_result(failures: &mut Vec<String>, invariant: &str, result: SurfaceReduceResult) {
    push_if_not_rejected(failures, invariant.to_owned(), result);
}

fn exercise_operation_generation_invariant(
    invariant: &str,
    index: usize,
    failures: &mut Vec<String>,
) {
    let seed = 51_000 + index as u32 * 100;
    match invariant {
        "AgentLoopOrdinalStrictlyIncreases" => {
            let mut operation = operation_record();
            operation.phase = OperationPhase::Admitted;
            let generation = generation_at(&operation, 0, GenerationPhase::Started, None);
            operation.generations.push(generation.clone());
            let initial = operation_state(operation, false);
            let turn = |ordinal| SurfaceAgentLoopTurn {
                turn_id: SurfaceTurnId::new(),
                fence: generation.fence.clone(),
                ordinal,
                task_id: SurfaceTaskId::try_new(format!("manifest-turn-{ordinal}")).unwrap(),
                task_status: SurfaceTaskRunningStatus::Running,
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(OperationPatch::AgentLoopTurnStarted { turn: turn(0) }),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(OperationPatch::AgentLoopTurnStarted { turn: turn(1) }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "BackgroundFenceMatchesTransferredGeneration" => {
            let operation = operation_record();
            let generation = generation_at(&operation, 0, GenerationPhase::Started, None);
            let initial = foreground_generation_state(operation, generation.clone());
            let transfer = |fence| OperationPatch::GenerationTransferred {
                fence: generation.fence.clone(),
                background_fence: fence,
                task_id: None,
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(transfer(background_fence(&generation.fence, 101))),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );
            let mut wrong_operation_fence = generation.fence.clone();
            wrong_operation_fence.generation_id = SurfaceGenerationId::new(1);
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(transfer(background_fence(
                        &wrong_operation_fence,
                        101,
                    ))),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "FirstGenerationIsZeroReserved" => {
            let operation = operation_record();
            let initial = operation_state(operation.clone(), true);
            let first = generation(&operation);
            let logical_turn_id = first.logical_turn_id.clone();
            let admitted = |first_generation| OperationPatch::Admitted {
                operation_id: operation.operation_id.clone(),
                logical_turn_id: logical_turn_id.clone(),
                input: AdmittedInput::NotApplicable,
                first_generation,
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(admitted(first.clone())),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );
            let mut nonzero = first;
            nonzero.fence.generation_id = SurfaceGenerationId::new(1);
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(admitted(nonzero)),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "GenerationIdsContiguous" => {
            let (operation, predecessor) = stopped_admitted_operation();
            let initial = operation_state(operation.clone(), false);
            let successor = generation_at(&operation, 1, GenerationPhase::Reserved, None);
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&successor),
                    SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                        generation: successor,
                    }),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );
            let mut skipped = generation_at(&operation, 2, GenerationPhase::Reserved, None);
            skipped.predecessor = Some(predecessor.fence);
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    generation_scope(&skipped),
                    SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                        generation: skipped,
                    }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "GoalGenerationIdentityMatchesGeneration" => {
            let mut operation = operation_record();
            let goal_id = SurfaceGoalId::try_new("manifest-goal").unwrap();
            let goal_run_id = SurfaceGoalRunId::try_new("manifest-goal-run").unwrap();
            operation.intent.kind = OperationKind::GoalRun {
                goal_id: goal_id.clone(),
                goal_run_id: goal_run_id.clone(),
                initial_objective_revision: GoalObjectiveRevision::new(1),
            };
            operation.phase = OperationPhase::Admitted;
            let predecessor = generation_at(
                &operation,
                0,
                GenerationPhase::Stopped,
                Some(GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                }),
            );
            operation.generations.push(predecessor);
            let mut successor = generation_at(&operation, 1, GenerationPhase::Reserved, None);
            let input_item_id = SurfaceItemId::new();
            let presentation = SurfaceInputPresentation::Visible {
                text: DisplayText::new("continue"),
            };
            let correlation_id =
                SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
            successor.input = GenerationInputState::Pending {
                input_item_id: input_item_id.clone(),
                presentation: presentation.clone(),
                correlation_id: correlation_id.clone(),
            };
            successor.goal_identity = Some(goal_generation_identity(
                &successor,
                goal_id,
                goal_run_id,
                2,
            ));
            let initial = operation_state(operation, false);
            let positive = batch(
                &initial,
                seed,
                vec![
                    (
                        generation_scope(&successor),
                        SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                            generation: successor.clone(),
                        }),
                    ),
                    (
                        generation_scope(&successor),
                        SurfaceEvent::Item(ItemPatch::Added {
                            item: SurfaceItem::UserMessage {
                                id: input_item_id,
                                turn_id: successor.logical_turn_id.clone(),
                                input: SurfaceUserInputState::Pending {
                                    presentation,
                                    correlation_id,
                                },
                                pinned: false,
                                origin: SurfaceItemOrigin::GoalContinuation,
                            },
                        }),
                    ),
                ],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );
            let mut mismatched = successor;
            mismatched.goal_identity.as_mut().unwrap().logical_turn_id = SurfaceTurnId::new();
            let invalid = batch(
                &initial,
                seed + 2,
                vec![(
                    generation_scope(&mismatched),
                    SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                        generation: mismatched,
                    }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "GoalPredecessorAuthorizesAtMostOneSuccessor" => {
            let predecessor = goal_identity(0, 1);
            let successor = goal_identity(1, 2);
            let initial = goal_continuation_state(&predecessor);
            let first = complete_goal_continuation_batch(&initial, seed, &predecessor, &successor);
            let after_first = assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &first),
            );

            let mut conflicting_successor = successor;
            conflicting_successor.goal_outer_turn_id =
                SurfaceGoalOuterTurnId::try_new("conflicting-successor").unwrap();
            let mut conflict = continuation_envelope(&predecessor, &conflicting_successor);
            conflict.receipt.goal_revision = GoalRevision::try_new(3).unwrap();
            conflict.receipt.store_commit_id = recorded_commit_id(seed + 1);
            conflict.receipt.receipt_digest = digest(151);
            let invalid = batch(
                &after_first,
                seed + 1,
                vec![(
                    SurfaceScope::Goal {
                        goal_id: predecessor.goal_id.clone(),
                        causative_generation: Some(predecessor.operation_fence.clone()),
                    },
                    SurfaceEvent::Goal(conflict),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &after_first, &invalid),
            );
        }
        "InputResolutionAfterStartedBeforeExecution" => {
            let mut operation = operation_record();
            operation.phase = OperationPhase::Admitted;
            let item_id = SurfaceItemId::new();
            let correlation_id =
                SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap();
            let mut generation = generation_at(&operation, 0, GenerationPhase::Started, None);
            generation.input = GenerationInputState::Pending {
                input_item_id: item_id.clone(),
                presentation: SurfaceInputPresentation::Visible {
                    text: DisplayText::new("resolve invariant"),
                },
                correlation_id: correlation_id.clone(),
            };
            operation.generations.push(generation.clone());
            let fact = SurfaceResolvedInputFact::NonReplayable {
                presentation: SurfaceInputPresentation::Visible {
                    text: DisplayText::new("resolve invariant"),
                },
                live_capsule_incarnation: incarnation(),
            };
            let mut active_snapshot = snapshot();
            active_snapshot.foreground_operation = Some(operation.clone());
            active_snapshot.items.push(SurfaceItem::UserMessage {
                id: item_id.clone(),
                turn_id: generation.logical_turn_id.clone(),
                input: SurfaceUserInputState::Pending {
                    presentation: SurfaceInputPresentation::Visible {
                        text: DisplayText::new("resolve invariant"),
                    },
                    correlation_id,
                },
                pinned: false,
                origin: SurfaceItemOrigin::UserInput,
            });
            let initial = SurfaceReducerState::new(active_snapshot);
            let resolution = |fence: SurfaceOperationFence| {
                vec![
                    (
                        SurfaceScope::Generation {
                            fence: fence.clone(),
                        },
                        SurfaceEvent::Operation(OperationPatch::InputBindingsResolved {
                            fence,
                            input_item_id: item_id.clone(),
                            fact: fact.clone(),
                        }),
                    ),
                    (
                        generation_scope(&generation),
                        SurfaceEvent::Item(ItemPatch::InputResolved {
                            item_id: item_id.clone(),
                            fact: fact.clone(),
                        }),
                    ),
                ]
            };
            let positive = batch(&initial, seed, resolution(generation.fence.clone()));
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );

            operation.generations[0].phase = GenerationPhase::Reserved;
            operation.generations[0].started_witness = None;
            let mut invalid_snapshot = initial.snapshot().clone();
            invalid_snapshot.foreground_operation = Some(operation);
            let invalid_state = SurfaceReducerState::new(invalid_snapshot);
            let invalid = batch(
                &invalid_state,
                seed + 2,
                resolution(generation.fence.clone()),
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &invalid_state, &invalid),
            );
        }
        "JoinFailedOnlyFromOperationJoinSettlement" => {
            let (operation, _) = stopped_admitted_operation();
            let terminal_seed = seed + 2;
            let finalize_intent_id =
                SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(terminal_seed + 1)).unwrap();
            let message = safe_message("operation join failed");
            let settlement_id =
                SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(seed + 3)).unwrap();
            let settlement_receipt = SurfaceSettlementReceipt {
                settlement_id: settlement_id.clone(),
                receipt_digest: digest(152),
            };
            let join_source = OperationJoinSettlementSource {
                operation_id: operation.operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                settlement_id: settlement_id.clone(),
                settlement_receipt_digest: settlement_receipt.receipt_digest.clone(),
                message: message.clone(),
            };
            let mut valid_operation = operation.clone();
            let valid_finalization = finalization_record(
                &valid_operation,
                terminal_seed,
                OperationFinalizationCause::OperationJoinSettlement(join_source),
                None,
                vec![settlement_id],
                vec![settlement_receipt.clone()],
            );
            let valid_intent = valid_finalization.finalize_intent_id.clone();
            valid_operation.phase = OperationPhase::Finalizing {
                finalize_intent_id: valid_intent.clone(),
            };
            valid_operation.finalization = Some(valid_finalization);
            let valid_state = operation_state(valid_operation, false);
            let terminal = |finalize_intent_id, settlement_receipts| {
                let mut record = invariant_terminal_record(
                    operation.operation_id.clone(),
                    finalize_intent_id,
                    OperationTerminal::JoinFailed {
                        message: message.clone(),
                    },
                    vector_usage(),
                );
                record.settlement_receipts = settlement_receipts;
                record
            };
            let positive = batch(
                &valid_state,
                terminal_seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: terminal(valid_intent, vec![settlement_receipt]),
                    }),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &valid_state, &positive),
            );

            let invalid_terminal_seed = seed + 12;
            let (invalid_state, invalid_intent) = finalizing_invariant_state(
                &operation,
                seed + 10,
                invalid_terminal_seed,
                OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                }),
            );
            let invalid = batch(
                &invalid_state,
                invalid_terminal_seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: terminal(invalid_intent, Vec::new()),
                    }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &invalid_state, &invalid),
            );
        }
        "LiveOperationHasNoTerminalRecord" => {
            let mut operation = operation_record();
            operation.phase = OperationPhase::Admitted;
            let generation = generation_at(&operation, 0, GenerationPhase::Started, None);
            operation.generations.push(generation.clone());
            let initial = operation_state(operation.clone(), false);
            let stopped = |fence| OperationPatch::GenerationStopped {
                fence,
                reason: GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                },
                usage_delta: vector_usage(),
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(stopped(generation.fence.clone())),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );

            operation.terminal = Some(invariant_terminal_record(
                operation.operation_id.clone(),
                SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(seed + 1)).unwrap(),
                OperationTerminal::Cancelled {
                    reason: CancelReason::User,
                },
                vector_usage(),
            ));
            let invalid_state = operation_state(operation, false);
            let invalid = batch(
                &invalid_state,
                seed + 2,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(stopped(generation.fence)),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &invalid_state, &invalid),
            );
        }
        "NoGenerationTransitionAfterStopped" => {
            let mut operation = operation_record();
            operation.phase = OperationPhase::Admitted;
            let generation = generation_at(&operation, 0, GenerationPhase::Started, None);
            operation.generations.push(generation.clone());
            let initial = operation_state(operation, false);
            let stop = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                        fence: generation.fence.clone(),
                        reason: GenerationStopReason::Completed {
                            status: GenerationCompletionStatus::Success,
                        },
                        usage_delta: vector_usage(),
                    }),
                )],
            );
            let stopped = assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &stop),
            );
            let invalid = batch(
                &stopped,
                seed + 1,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(OperationPatch::GenerationStarted {
                        fence: generation.fence.clone(),
                        witness: generation_started_witness(seed + 1, &generation.replayability),
                    }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &stopped, &invalid),
            );
        }
        "PredecessorStoppedBeforeSuccessorReserved" => {
            let (operation, _) = stopped_admitted_operation();
            let successor = generation_at(&operation, 1, GenerationPhase::Reserved, None);
            let valid_state = operation_state(operation.clone(), false);
            let positive = batch(
                &valid_state,
                seed,
                vec![(
                    generation_scope(&successor),
                    SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                        generation: successor.clone(),
                    }),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &valid_state, &positive),
            );

            let mut live_predecessor = operation;
            live_predecessor.generations[0].phase = GenerationPhase::Started;
            live_predecessor.generations[0].stop_reason = None;
            let invalid_state = operation_state(live_predecessor, false);
            let invalid = batch(
                &invalid_state,
                seed + 1,
                vec![(
                    generation_scope(&successor),
                    SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                        generation: successor,
                    }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &invalid_state, &invalid),
            );
        }
        "SameOperationThreadAndOwnerEpoch" => {
            let (operation, _) = stopped_admitted_operation();
            let successor = generation_at(&operation, 1, GenerationPhase::Reserved, None);
            let initial = operation_state(operation, false);
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&successor),
                    SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                        generation: successor.clone(),
                    }),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );
            let mut wrong_owner = successor;
            wrong_owner.fence.thread_owner_epoch = ThreadOwnerEpoch::new(2);
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    generation_scope(&wrong_owner),
                    SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                        generation: wrong_owner,
                    }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "StartedFreezesSettingsPolicyReplayability" => {
            let operation = operation_record();
            let generation = generation_at(&operation, 0, GenerationPhase::Reserved, None);
            let initial = foreground_generation_state(operation, generation.clone());
            let witness = generation_started_witness(seed, &generation.replayability);
            let positive = batch(
                &initial,
                seed,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(OperationPatch::GenerationStarted {
                        fence: generation.fence.clone(),
                        witness: witness.clone(),
                    }),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );
            let mut changed = witness;
            changed.durable_replayability_digest = digest(153);
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    generation_scope(&generation),
                    SurfaceEvent::Operation(OperationPatch::GenerationStarted {
                        fence: generation.fence.clone(),
                        witness: changed,
                    }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "SuspendedNamesExactStoppedGeneration" => {
            let mut operation = operation_record();
            operation.phase = OperationPhase::Admitted;
            let generation = generation_at(
                &operation,
                0,
                GenerationPhase::Stopped,
                Some(GenerationStopReason::InterruptedResumable),
            );
            operation.generations.push(generation.clone());
            operation.pending_control = Some(PendingControlIntent::Interrupt {
                generation_fence: generation.fence.clone(),
            });
            let initial = operation_state(operation.clone(), false);
            let suspended = |generation_id| OperationPatch::Suspended {
                operation_id: operation.operation_id.clone(),
                cause: SuspensionCause::Interrupted { generation_id },
            };
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(suspended(generation.fence.generation_id)),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
            );
            let invalid = batch(
                &initial,
                seed + 1,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(suspended(SurfaceGenerationId::new(1))),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            );
        }
        "TerminalHasExactlyOneRecord" => {
            let (operation, _) = stopped_admitted_operation();
            let terminal_seed = seed + 2;
            let (finalizing, finalize_intent_id) = finalizing_invariant_state(
                &operation,
                seed,
                terminal_seed,
                OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                }),
            );
            let record = invariant_terminal_record(
                operation.operation_id.clone(),
                finalize_intent_id,
                OperationTerminal::Succeeded {
                    usage: vector_usage(),
                },
                vector_usage(),
            );
            let positive = batch(
                &finalizing,
                terminal_seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: record.clone(),
                    }),
                )],
            );
            let terminal = assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &finalizing, &positive),
            );
            assert_eq!(
                terminal
                    .snapshot()
                    .operation_history
                    .iter()
                    .filter(|history| history.operation_id == operation.operation_id)
                    .count(),
                1
            );
            let invalid = batch(
                &terminal,
                terminal_seed + 1,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal { record }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &terminal, &invalid),
            );
        }
        "TerminalUsageMatchesRecord" => {
            let (operation, _) = stopped_admitted_operation();
            let terminal_seed = seed + 2;
            let (finalizing, finalize_intent_id) = finalizing_invariant_state(
                &operation,
                seed,
                terminal_seed,
                OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
                    status: GenerationCompletionStatus::Success,
                }),
            );
            let terminal_record = |terminal_usage, record_usage| {
                invariant_terminal_record(
                    operation.operation_id.clone(),
                    finalize_intent_id.clone(),
                    OperationTerminal::Succeeded {
                        usage: terminal_usage,
                    },
                    record_usage,
                )
            };
            let positive = batch(
                &finalizing,
                terminal_seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: terminal_record(vector_usage(), vector_usage()),
                    }),
                )],
            );
            assert_vector_applied(
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &finalizing, &positive),
            );
            let invalid = batch(
                &finalizing,
                terminal_seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: terminal_record(usage(), vector_usage()),
                    }),
                )],
            );
            invariant_result(
                failures,
                invariant,
                reduce_batch(SurfaceReduceMode::Live, &finalizing, &invalid),
            );
        }
        unknown => panic!("unrecognized operation_generation_invariant: {unknown}"),
    }
}

#[test]
fn operation_generation_invariants_execute_every_manifest_name_once() {
    const EXPECTED: [&str; 16] = [
        "AgentLoopOrdinalStrictlyIncreases",
        "BackgroundFenceMatchesTransferredGeneration",
        "FirstGenerationIsZeroReserved",
        "GenerationIdsContiguous",
        "GoalGenerationIdentityMatchesGeneration",
        "GoalPredecessorAuthorizesAtMostOneSuccessor",
        "InputResolutionAfterStartedBeforeExecution",
        "JoinFailedOnlyFromOperationJoinSettlement",
        "LiveOperationHasNoTerminalRecord",
        "NoGenerationTransitionAfterStopped",
        "PredecessorStoppedBeforeSuccessorReserved",
        "SameOperationThreadAndOwnerEpoch",
        "StartedFreezesSettingsPolicyReplayability",
        "SuspendedNamesExactStoppedGeneration",
        "TerminalHasExactlyOneRecord",
        "TerminalUsageMatchesRecord",
    ];
    let invariants = manifest_string_inventory("operation_generation_invariants");
    assert_eq!(
        invariants.len(),
        EXPECTED.len(),
        "operation/generation invariant count drifted"
    );
    assert_eq!(
        invariants.iter().cloned().collect::<BTreeSet<_>>(),
        EXPECTED.into_iter().map(str::to_owned).collect(),
        "operation/generation invariant inventory drifted"
    );

    let mut consumed = BTreeSet::new();
    let mut failures = Vec::new();
    for (index, invariant) in invariants.iter().enumerate() {
        assert!(
            consumed.insert(invariant.clone()),
            "duplicate operation/generation invariant: {invariant}"
        );
        exercise_operation_generation_invariant(invariant, index, &mut failures);
    }
    assert_eq!(consumed.len(), invariants.len());
    assert!(
        failures.is_empty(),
        "operation/generation invariant mutations unexpectedly applied ({}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn operation_terminal_must_match_the_selected_generation_stop_cause() {
    let mut operation = operation_record();
    operation.phase = OperationPhase::Admitted;
    let message = safe_message("verification failed");
    operation.generations.push(generation_at(
        &operation,
        0,
        GenerationPhase::Stopped,
        Some(GenerationStopReason::Completed {
            status: GenerationCompletionStatus::VerificationFailed {
                message: message.clone(),
            },
        }),
    ));
    let terminal_seed = 58_002;
    let selected_cause =
        OperationFinalizationCause::GenerationStop(GenerationStopReason::Completed {
            status: GenerationCompletionStatus::VerificationFailed { message },
        });
    let (finalizing, finalize_intent_id) =
        finalizing_invariant_state(&operation, 58_000, terminal_seed, selected_cause);
    let invalid = batch(
        &finalizing,
        terminal_seed,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::Terminal {
                record: invariant_terminal_record(
                    operation.operation_id,
                    finalize_intent_id,
                    OperationTerminal::Succeeded {
                        usage: vector_usage(),
                    },
                    vector_usage(),
                ),
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &finalizing, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn operation_terminal_mapping_executes_every_manifest_row_once() {
    let rows = manifest_operation_terminal_rows();
    assert_eq!(
        rows.len(),
        65,
        "operation terminal mapping row count drifted"
    );

    let mut consumed = BTreeSet::new();
    let mut terminal_rows = 0;
    let mut nonterminal_rows = 0;
    let mut failures = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let label = format!("{}|{}|{}", row.source, row.target, row.condition);
        assert!(
            consumed.insert(label.clone()),
            "duplicate operation terminal mapping row: {label}"
        );
        let seed = 59_000 + index as u32 * 20;
        let Some(fixtures) = operation_terminal_fixtures(row, seed) else {
            nonterminal_rows += 1;
            continue;
        };
        terminal_rows += 1;
        assert!(
            !fixtures.is_empty(),
            "terminal row has no fixtures: {label}"
        );

        for (fixture_index, fixture) in fixtures.into_iter().enumerate() {
            let mut operation = fixture.operation;
            let finalization = finalization_record(
                &operation,
                seed,
                fixture.selected_cause,
                fixture.suspended_cause,
                fixture.expected_settlements,
                fixture.settlement_receipts.clone(),
            );
            operation.phase = OperationPhase::Finalizing {
                finalize_intent_id: finalization.finalize_intent_id.clone(),
            };
            operation.finalization = Some(finalization);
            let operation_id = operation.operation_id.clone();
            let initial = operation_state(operation.clone(), false);
            let record = terminal_record_for(
                &operation,
                fixture.terminal.clone(),
                vector_usage(),
                fixture.settlement_receipts,
            );
            let positive = batch(
                &initial,
                seed,
                vec![(
                    operation_scope(&operation),
                    SurfaceEvent::Operation(OperationPatch::Terminal {
                        record: record.clone(),
                    }),
                )],
            );
            if !matches!(
                reduce_batch(SurfaceReduceMode::Live, &initial, &positive),
                SurfaceReduceResult::Applied { .. }
            ) {
                failures.push(format!("{label}:fixture-{fixture_index}:positive"));
            }

            let mut wrong = record;
            wrong.terminal = mismatched_terminal(&wrong.terminal);
            let complement = batch(
                &initial,
                seed,
                vec![(
                    SurfaceScope::Operation {
                        operation_id: operation_id.clone(),
                    },
                    SurfaceEvent::Operation(OperationPatch::Terminal { record: wrong }),
                )],
            );
            if !matches!(
                reduce_batch(SurfaceReduceMode::Live, &initial, &complement),
                SurfaceReduceResult::Rejected { .. }
            ) {
                failures.push(format!("{label}:fixture-{fixture_index}:complement"));
            }
        }
    }

    assert_eq!(terminal_rows, 52, "terminal row classification drifted");
    assert_eq!(
        nonterminal_rows, 13,
        "nonterminal row classification drifted"
    );
    assert_eq!(consumed.len(), rows.len());
    assert!(
        failures.is_empty(),
        "operation terminal mapping failures ({}):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn state_with_foreground_operation(mut operation: OperationRecord) -> SurfaceReducerState {
    operation.phase = OperationPhase::Admitted;
    let mut snapshot = snapshot();
    snapshot.foreground_operation = Some(operation);
    SurfaceReducerState::new(snapshot)
}

#[test]
fn remaining_operation_control_and_execution_facts_are_reducible() {
    let operation = operation_record();
    let requested = batch(
        &state(),
        16_000,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::Requested {
                operation: operation.clone(),
            }),
        )],
    );
    let queued = applied(reduce_batch(SurfaceReduceMode::Live, &state(), &requested));
    let queue_changed = batch(
        &queued,
        16_001,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::ReservationQueueChanged {
                operation_id: operation.operation_id.clone(),
                reservation_sequence: SequenceNumber::new(2),
                ready_for_admission: false,
                queue_position: 0,
            }),
        )],
    );
    let requeued = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &queued,
        &queue_changed,
    ));
    assert_eq!(
        requeued.snapshot().queued_operations[0]
            .reservation
            .reservation_sequence
            .get(),
        2
    );

    let mut active = operation.clone();
    let mut active_generation = generation(&active);
    active_generation.phase = GenerationPhase::Started;
    active_generation.started_witness = Some(GenerationStartedWitness {
        started_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(16_002)).unwrap(),
        settings_revision: SettingsRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        durable_replayability_digest: digest(100),
        capability_fingerprint: active_generation.capability_fingerprint.clone(),
    });
    active.initial_logical_turn_id = Some(active_generation.logical_turn_id.clone());
    active.generations.push(active_generation.clone());
    let active_state = state_with_foreground_operation(active.clone());
    let control = batch(
        &active_state,
        16_003,
        vec![(
            operation_scope(&active),
            SurfaceEvent::Operation(OperationPatch::ControlIntentCommitted {
                operation_id: active.operation_id.clone(),
                request_id: active.request_id.clone(),
                intent: PendingControlIntent::Interrupt {
                    generation_fence: active_generation.fence.clone(),
                },
            }),
        )],
    );
    let controlled = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &active_state,
        &control,
    ));
    let turn = SurfaceAgentLoopTurn {
        turn_id: active_generation.logical_turn_id.clone(),
        fence: active_generation.fence.clone(),
        ordinal: 0,
        task_id: SurfaceTaskId::try_new("main-turn").unwrap(),
        task_status: SurfaceTaskRunningStatus::Running,
    };
    let execution_facts = batch(
        &controlled,
        16_004,
        vec![
            (
                generation_scope(&active_generation),
                SurfaceEvent::Operation(OperationPatch::AgentLoopTurnStarted {
                    turn: turn.clone(),
                }),
            ),
            (
                generation_scope(&active_generation),
                SurfaceEvent::Operation(OperationPatch::ModelRouteSelected {
                    fence: active_generation.fence.clone(),
                    requested_model: NonEmptyText::try_new("deepseek-v4").unwrap(),
                    actual_model: NonEmptyText::try_new("deepseek-v4-fast").unwrap(),
                    reason: NonEmptyText::try_new("availability").unwrap(),
                }),
            ),
            (
                generation_scope(&active_generation),
                SurfaceEvent::Operation(OperationPatch::VerificationStarted {
                    fence: active_generation.fence.clone(),
                    verification_id: UuidV7::try_from_bytes(uuid_v7_bytes(16_005)).unwrap(),
                    command: NonEmptyText::try_new("cargo test").unwrap(),
                }),
            ),
            (
                generation_scope(&active_generation),
                SurfaceEvent::Operation(OperationPatch::VerificationCompleted {
                    fence: active_generation.fence.clone(),
                    verification_id: UuidV7::try_from_bytes(uuid_v7_bytes(16_005)).unwrap(),
                    result: SurfaceVerificationResult {
                        command: NonEmptyText::try_new("cargo test").unwrap(),
                        success: true,
                        exit_code: Some(0),
                        stdout: DisplayText::new("ok"),
                        stderr: DisplayText::new(""),
                    },
                }),
            ),
        ],
    );
    let executed = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &controlled,
        &execution_facts,
    ));
    assert_eq!(
        executed
            .snapshot()
            .foreground_operation
            .as_ref()
            .unwrap()
            .agent_loop_turns,
        vec![turn]
    );
}

#[test]
fn remaining_operation_generation_suspension_and_finalization_patches_reduce() {
    let mut operation = operation_record();
    let mut first_generation = generation(&operation);
    first_generation.phase = GenerationPhase::Started;
    first_generation.input = GenerationInputState::Pending {
        input_item_id: SurfaceItemId::new(),
        presentation: SurfaceInputPresentation::Visible {
            text: DisplayText::new("resolve"),
        },
        correlation_id: SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(16_100)).unwrap(),
    };
    first_generation.started_witness = Some(GenerationStartedWitness {
        started_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(16_101)).unwrap(),
        settings_revision: SettingsRevision::try_new(1).unwrap(),
        policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        durable_replayability_digest: digest(101),
        capability_fingerprint: first_generation.capability_fingerprint.clone(),
    });
    operation.initial_logical_turn_id = Some(first_generation.logical_turn_id.clone());
    operation.initial_input_item_id = match &first_generation.input {
        GenerationInputState::Pending { input_item_id, .. } => Some(input_item_id.clone()),
        _ => unreachable!(),
    };
    operation.generations.push(first_generation.clone());
    let mut active_snapshot = snapshot();
    active_snapshot.items.push(SurfaceItem::UserMessage {
        id: operation.initial_input_item_id.clone().unwrap(),
        turn_id: first_generation.logical_turn_id.clone(),
        input: SurfaceUserInputState::Pending {
            presentation: SurfaceInputPresentation::Visible {
                text: DisplayText::new("resolve"),
            },
            correlation_id: SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(16_100))
                .unwrap(),
        },
        pinned: false,
        origin: SurfaceItemOrigin::UserInput,
    });
    active_snapshot.foreground_operation = Some({
        operation.phase = OperationPhase::Admitted;
        operation.clone()
    });
    let active_state = SurfaceReducerState::new(active_snapshot);
    let fact = SurfaceResolvedInputFact::NonReplayable {
        presentation: SurfaceInputPresentation::Visible {
            text: DisplayText::new("resolve"),
        },
        live_capsule_incarnation: incarnation(),
    };
    let resolved = batch(
        &active_state,
        16_102,
        vec![
            (
                generation_scope(&first_generation),
                SurfaceEvent::Operation(OperationPatch::InputBindingsResolved {
                    fence: first_generation.fence.clone(),
                    input_item_id: operation.initial_input_item_id.clone().unwrap(),
                    fact: fact.clone(),
                }),
            ),
            (
                generation_scope(&first_generation),
                SurfaceEvent::Item(ItemPatch::InputResolved {
                    item_id: operation.initial_input_item_id.clone().unwrap(),
                    fact,
                }),
            ),
        ],
    );
    let resolved_state = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &active_state,
        &resolved,
    ));

    let stop_first = batch(
        &resolved_state,
        16_103,
        vec![
            (
                operation_scope(&operation),
                SurfaceEvent::Operation(OperationPatch::ControlIntentCommitted {
                    operation_id: operation.operation_id.clone(),
                    request_id: operation.request_id.clone(),
                    intent: PendingControlIntent::Interrupt {
                        generation_fence: first_generation.fence.clone(),
                    },
                }),
            ),
            (
                generation_scope(&first_generation),
                SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                    fence: first_generation.fence.clone(),
                    reason: GenerationStopReason::InterruptedResumable,
                    usage_delta: usage(),
                }),
            ),
        ],
    );
    let stopped = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &resolved_state,
        &stop_first,
    ));
    let suspended = batch(
        &stopped,
        16_104,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::Suspended {
                operation_id: operation.operation_id.clone(),
                cause: SuspensionCause::Interrupted {
                    generation_id: first_generation.fence.generation_id,
                },
            }),
        )],
    );
    let suspended_state = applied(reduce_batch(SurfaceReduceMode::Live, &stopped, &suspended));

    let mut replacement = first_generation.clone();
    replacement.fence.generation_id = SurfaceGenerationId::new(1);
    replacement.predecessor = Some(first_generation.fence.clone());
    replacement.phase = GenerationPhase::Reserved;
    replacement.started_witness = None;
    replacement.stop_reason = None;
    let reserved = batch(
        &suspended_state,
        16_105,
        vec![(
            generation_scope(&replacement),
            SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                generation: replacement.clone(),
            }),
        )],
    );
    let reserved_state = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &suspended_state,
        &reserved,
    ));
    let control = batch(
        &reserved_state,
        16_106,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::ControlIntentCommitted {
                operation_id: operation.operation_id.clone(),
                request_id: operation.request_id.clone(),
                intent: PendingControlIntent::ResumeStarting {
                    generation_fence: replacement.fence.clone(),
                },
            }),
        )],
    );
    let resume_starting = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &reserved_state,
        &control,
    ));
    let stop_replacement = batch(
        &resume_starting,
        16_107,
        vec![(
            generation_scope(&replacement),
            SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                fence: replacement.fence.clone(),
                reason: GenerationStopReason::NotStarted {
                    reason: NotStartedReason::Interrupted,
                },
                usage_delta: usage(),
            }),
        )],
    );
    let replacement_stopped = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &resume_starting,
        &stop_replacement,
    ));
    let rebased_cause = SuspensionCause::Interrupted {
        generation_id: replacement.fence.generation_id,
    };
    let rebase = batch(
        &replacement_stopped,
        16_108,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::SuspensionRebasedAfterUnstartedResume {
                operation_id: operation.operation_id.clone(),
                previous_cause: SuspensionCause::Interrupted {
                    generation_id: first_generation.fence.generation_id,
                },
                replacement_fence: replacement.fence.clone(),
                rebased_cause: rebased_cause.clone(),
            }),
        )],
    );
    let rebased = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &replacement_stopped,
        &rebase,
    ));
    assert!(matches!(
        &rebased.snapshot().foreground_operation.as_ref().unwrap().phase,
        OperationPhase::Suspended { cause } if cause == &rebased_cause
    ));

    let settlement_a = SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(16_109)).unwrap();
    let settlement_b = SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(16_110)).unwrap();
    let finalize_intent_id =
        SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(16_111)).unwrap();
    let finalization = batch(
        &rebased,
        16_112,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                operation_id: operation.operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                terminal_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(16_113)).unwrap(),
                selected_cause: OperationFinalizationCause::Suspended(
                    SuspendedFinalizationCause::ResumeStartCommitFailure {
                        message: SafeDiagnosticText::try_new("resume failed").unwrap(),
                    },
                ),
                suspended_cause: Some(SuspendedFinalizationCause::ResumeStartCommitFailure {
                    message: SafeDiagnosticText::try_new("resume failed").unwrap(),
                }),
                expected_settlements: vec![settlement_a.clone(), settlement_b.clone()],
            }),
        )],
    );
    let finalizing = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &rebased,
        &finalization,
    ));
    let settle = batch(
        &finalizing,
        16_114,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::FinalizationSettlementRecorded {
                operation_id: operation.operation_id.clone(),
                finalize_intent_id: finalize_intent_id.clone(),
                receipt: SurfaceSettlementReceipt {
                    settlement_id: settlement_a,
                    receipt_digest: digest(102),
                },
            }),
        )],
    );
    let partially_settled = applied(reduce_batch(SurfaceReduceMode::Live, &finalizing, &settle));
    let degraded = batch(
        &partially_settled,
        16_115,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::FinalizationDegraded {
                operation_id: operation.operation_id,
                finalize_intent_id,
                cause: FinalizationDegradedCause::MissingFinalization {
                    terminal_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(16_113))
                        .unwrap(),
                    missing_settlements: NonEmptyVec::try_new(vec![settlement_b]).unwrap(),
                    missing_set_digest: digest(103),
                },
                last_error: DisplayText::new("settlement missing"),
            }),
        )],
    );
    let degraded_state = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &partially_settled,
        &degraded,
    ));
    assert!(matches!(
        degraded_state
            .snapshot()
            .foreground_operation
            .as_ref()
            .unwrap()
            .phase,
        OperationPhase::FinalizingDegraded { .. }
    ));
}

fn goal_usage() -> GoalUsage {
    GoalUsage {
        charged_input_tokens: 0,
        output_tokens: 0,
        cache_tokens: 0,
        verifier_tokens: 0,
        cost_micros: 0,
        elapsed_seconds: 0,
    }
}

fn goal_identity(generation_id: u64, outer_turn_count: u32) -> SurfaceGoalGenerationIdentity {
    SurfaceGoalGenerationIdentity {
        goal_id: SurfaceGoalId::try_new("goal-continuation").unwrap(),
        goal_run_id: SurfaceGoalRunId::try_new("goal-run").unwrap(),
        operation_fence: SurfaceOperationFence {
            thread_id: thread_id(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(12_000)).unwrap(),
            generation_id: SurfaceGenerationId::new(generation_id),
        },
        goal_outer_turn_id: SurfaceGoalOuterTurnId::try_new(format!("outer-{generation_id}"))
            .unwrap(),
        logical_turn_id: SurfaceTurnId::new(),
        canonical_input_item_id: SurfaceItemId::new(),
        outer_turn_origin: if generation_id == 0 {
            GoalOuterTurnOrigin::User
        } else {
            GoalOuterTurnOrigin::Continuation
        },
        attempt: GenerationAttempt::Initial,
        predecessor_fence: (generation_id > 0).then(|| SurfaceOperationFence {
            thread_id: thread_id(),
            thread_owner_epoch: ThreadOwnerEpoch::new(1),
            operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(12_000)).unwrap(),
            generation_id: SurfaceGenerationId::new(generation_id - 1),
        }),
        objective_revision: GoalObjectiveRevision::new(1),
        outer_turn_count,
    }
}

fn goal_operation(identity: &SurfaceGoalGenerationIdentity) -> OperationRecord {
    let mut operation = operation_record();
    operation.operation_id = identity.operation_fence.operation_id.clone();
    operation.reservation = reservation_lease(&operation.operation_id);
    operation.intent.kind = OperationKind::GoalRun {
        goal_id: identity.goal_id.clone(),
        goal_run_id: identity.goal_run_id.clone(),
        initial_objective_revision: identity.objective_revision,
    };
    operation.phase = OperationPhase::Admitted;
    operation.initial_logical_turn_id = Some(identity.logical_turn_id.clone());
    operation.initial_input_item_id = Some(identity.canonical_input_item_id.clone());
    let mut generation = generation(&operation);
    generation.fence = identity.operation_fence.clone();
    generation.logical_turn_id = identity.logical_turn_id.clone();
    generation.input = GenerationInputState::Resolved {
        input_item_id: identity.canonical_input_item_id.clone(),
        fact: SurfaceResolvedInputFact::NonReplayable {
            presentation: SurfaceInputPresentation::Visible {
                text: DisplayText::new("goal input"),
            },
            live_capsule_incarnation: incarnation(),
        },
    };
    generation.predecessor = identity.predecessor_fence.clone();
    generation.attempt = identity.attempt;
    generation.goal_identity = Some(identity.clone());
    generation.phase = GenerationPhase::Started;
    generation.started_witness = Some(generation_started_witness(
        17_090,
        &generation.replayability,
    ));
    operation.generations.push(generation);
    operation
}

fn goal_with_predecessor(predecessor: &SurfaceGoalGenerationIdentity) -> SurfaceGoal {
    SurfaceGoal {
        goal_id: predecessor.goal_id.clone(),
        thread_id: thread_id(),
        goal_revision: GoalRevision::try_new(1).unwrap(),
        goal_owner_epoch: GoalOwnerEpoch::try_new(1).unwrap(),
        catalog_revision: GoalCatalogRevision::try_new(1).unwrap(),
        receipt_digest: digest(1),
        objective: NonEmptyText::try_new("finish reducer").unwrap(),
        objective_revision: GoalObjectiveRevision::new(1),
        state: SurfaceGoalState::Active,
        token_budget: Some(100),
        usage: goal_usage(),
        current_run: Some(SurfaceGoalRun {
            goal_run_id: predecessor.goal_run_id.clone(),
            run_origin: SurfaceGoalRunOrigin::User,
            operation_id: predecessor.operation_fence.operation_id.clone(),
            phase: SurfaceGoalRunPhase::InFlight {
                outer_turn: SurfaceGoalOuterTurnReceipt {
                    outer_turn_id: predecessor.goal_outer_turn_id.clone(),
                    origin: SurfaceGoalOuterTurnReceiptOrigin::User,
                    outer_turn_count: predecessor.outer_turn_count,
                },
            },
        }),
        last_transition: None,
    }
}

fn simple_goal() -> SurfaceGoal {
    let identity = goal_identity(0, 1);
    let mut goal = goal_with_predecessor(&identity);
    goal.current_run = None;
    goal
}

fn evidence() -> SurfaceEvidenceItem {
    SurfaceEvidenceItem {
        kind: SurfaceEvidenceKind::Test,
        summary: NonEmptyText::try_new("reducer tests pass").unwrap(),
        target: Some(DisplayText::new("runtime_surface_reducer")),
    }
}

fn goal_receipt(
    goal: &SurfaceGoal,
    revision: u64,
    catalog_revision: u64,
    row_state: SurfaceGoalReceiptState,
    seed: u32,
) -> SurfaceGoalStoreReceipt {
    SurfaceGoalStoreReceipt {
        goal_id: goal.goal_id.clone(),
        goal_revision: GoalRevision::try_new(revision).unwrap(),
        objective_revision: goal.objective_revision,
        catalog_revision: GoalCatalogRevision::try_new(catalog_revision).unwrap(),
        goal_owner_epoch: goal.goal_owner_epoch,
        row_state,
        store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
        receipt_digest: digest((seed % 251) as u8),
    }
}

fn goal_event(receipt: SurfaceGoalStoreReceipt, patch: GoalPatch) -> (SurfaceScope, SurfaceEvent) {
    let causative_generation = match &patch {
        GoalPatch::OuterTurnStarted { identity }
        | GoalPatch::OuterTurnFinished { identity, .. }
        | GoalPatch::VerificationCompleted { identity, .. } => {
            Some(identity.operation_fence.clone())
        }
        GoalPatch::ContinuationDecided { predecessor, .. } => {
            Some(predecessor.operation_fence.clone())
        }
        _ => None,
    };
    (
        SurfaceScope::Goal {
            goal_id: receipt.goal_id.clone(),
            causative_generation,
        },
        SurfaceEvent::Goal(GoalPatchEnvelope { receipt, patch }),
    )
}

#[test]
fn goal_create_edit_and_remove_are_receipt_backed() {
    let goal = simple_goal();
    let created_receipt = goal_receipt(
        &goal,
        1,
        1,
        SurfaceGoalReceiptState::Present {
            state: goal.state.clone(),
            current_run: None,
        },
        17_000,
    );
    let created_batch = batch(
        &state(),
        17_001,
        vec![goal_event(
            created_receipt,
            GoalPatch::Created { goal: goal.clone() },
        )],
    );
    let created = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &state(),
        &created_batch,
    ));

    let mut edited_goal = goal.clone();
    edited_goal.goal_revision = GoalRevision::try_new(2).unwrap();
    edited_goal.objective = NonEmptyText::try_new("finish all reducer patches").unwrap();
    edited_goal.objective_revision = GoalObjectiveRevision::new(2);
    let edited_receipt = goal_receipt(
        &edited_goal,
        2,
        1,
        SurfaceGoalReceiptState::Present {
            state: SurfaceGoalState::Active,
            current_run: None,
        },
        17_002,
    );
    let edited_batch = batch(
        &created,
        17_003,
        vec![goal_event(
            edited_receipt,
            GoalPatch::Edited {
                goal_id: goal.goal_id.clone(),
                previous_revision: GoalRevision::try_new(1).unwrap(),
                goal: edited_goal.clone(),
            },
        )],
    );
    let edited = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &created,
        &edited_batch,
    ));

    let removed_receipt = goal_receipt(
        &edited_goal,
        3,
        2,
        SurfaceGoalReceiptState::Removed {
            tombstone_revision: GoalRevision::try_new(3).unwrap(),
        },
        17_004,
    );
    let removed_batch = batch(
        &edited,
        17_005,
        vec![goal_event(
            removed_receipt,
            GoalPatch::Removed {
                goal_id: goal.goal_id,
                previous_revision: GoalRevision::try_new(2).unwrap(),
                tombstone_revision: GoalRevision::try_new(3).unwrap(),
            },
        )],
    );
    let removed = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &edited,
        &removed_batch,
    ));
    assert!(removed.snapshot().goal.is_none());
}

#[test]
fn goal_run_intent_and_transition_patches_follow_receipt_state() {
    let identity = goal_identity(0, 1);
    let goal = simple_goal();
    let mut snapshot = snapshot();
    snapshot.goal = Some(goal.clone());
    snapshot.foreground_operation = Some(goal_operation(&identity));
    let initial = SurfaceReducerState::new(snapshot);
    let preparing_run = SurfaceGoalRun {
        goal_run_id: identity.goal_run_id.clone(),
        run_origin: SurfaceGoalRunOrigin::User,
        operation_id: identity.operation_fence.operation_id.clone(),
        phase: SurfaceGoalRunPhase::Preparing,
    };
    let run_started = batch(
        &initial,
        17_100,
        vec![goal_event(
            goal_receipt(
                &goal,
                2,
                1,
                SurfaceGoalReceiptState::Present {
                    state: SurfaceGoalState::Active,
                    current_run: Some(preparing_run.clone()),
                },
                17_101,
            ),
            GoalPatch::RunStarted {
                goal_id: goal.goal_id.clone(),
                goal_run: preparing_run.clone(),
            },
        )],
    );
    let preparing = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &initial,
        &run_started,
    ));

    let outer_turn = SurfaceGoalOuterTurnReceipt {
        outer_turn_id: identity.goal_outer_turn_id.clone(),
        origin: SurfaceGoalOuterTurnReceiptOrigin::User,
        outer_turn_count: 1,
    };
    let in_flight_run = SurfaceGoalRun {
        phase: SurfaceGoalRunPhase::InFlight {
            outer_turn: outer_turn.clone(),
        },
        ..preparing_run.clone()
    };
    let outer_started = batch(
        &preparing,
        17_102,
        vec![goal_event(
            goal_receipt(
                &goal,
                3,
                1,
                SurfaceGoalReceiptState::Present {
                    state: SurfaceGoalState::Active,
                    current_run: Some(in_flight_run.clone()),
                },
                17_103,
            ),
            GoalPatch::OuterTurnStarted {
                identity: identity.clone(),
            },
        )],
    );

    let mut missing_generation_snapshot = preparing.snapshot().clone();
    missing_generation_snapshot.foreground_operation = None;
    let missing_generation = SurfaceReducerState::new(missing_generation_snapshot);
    let invalid_outer_started = batch(
        &missing_generation,
        17_099,
        outer_started
            .events
            .as_slice()
            .iter()
            .map(|event| (event.scope.clone(), event.event.clone()))
            .collect(),
    );
    rejected(
        reduce_batch(
            SurfaceReduceMode::Live,
            &missing_generation,
            &invalid_outer_started,
        ),
        SurfaceReducerErrorCode::MissingIdentity,
    );
    let in_flight = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &preparing,
        &outer_started,
    ));

    let intent = SurfaceGoalIntent::Complete {
        intent_id: SurfaceGoalIntentId::try_new("intent-complete").unwrap(),
        reason: NonEmptyText::try_new("verified").unwrap(),
        evidence: NonEmptyVec::try_new(vec![evidence()]).unwrap(),
    };
    let requested = batch(
        &in_flight,
        17_104,
        vec![goal_event(
            goal_receipt(
                &goal,
                4,
                1,
                SurfaceGoalReceiptState::Present {
                    state: SurfaceGoalState::Active,
                    current_run: Some(in_flight_run.clone()),
                },
                17_105,
            ),
            GoalPatch::IntentRequested {
                goal_id: goal.goal_id.clone(),
                outer_turn_id: identity.goal_outer_turn_id.clone(),
                intent: intent.clone(),
            },
        )],
    );
    let intent_requested = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &in_flight,
        &requested,
    ));
    let acknowledged = batch(
        &intent_requested,
        17_106,
        vec![goal_event(
            goal_receipt(
                &goal,
                5,
                1,
                SurfaceGoalReceiptState::Present {
                    state: SurfaceGoalState::Active,
                    current_run: Some(in_flight_run.clone()),
                },
                17_107,
            ),
            GoalPatch::IntentAcknowledged {
                goal_id: goal.goal_id.clone(),
                outer_turn_id: identity.goal_outer_turn_id.clone(),
                intent,
                ack: SurfaceGoalIntentAck::DeferredToTurnEnd {
                    intent_id: SurfaceGoalIntentId::try_new("intent-complete").unwrap(),
                    pending_depth: 1,
                },
            },
        )],
    );
    let intent_acknowledged = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &intent_requested,
        &acknowledged,
    ));

    let settled_run = SurfaceGoalRun {
        phase: SurfaceGoalRunPhase::Settled {
            last_outer_turn: Some(outer_turn),
        },
        ..preparing_run
    };
    let finished = batch(
        &intent_acknowledged,
        17_108,
        vec![goal_event(
            goal_receipt(
                &goal,
                6,
                1,
                SurfaceGoalReceiptState::Present {
                    state: SurfaceGoalState::Active,
                    current_run: Some(settled_run.clone()),
                },
                17_109,
            ),
            GoalPatch::OuterTurnFinished {
                identity: identity.clone(),
                status: GoalOuterTurnStatus::Success,
                usage: goal_usage(),
                next_action: GoalOuterTurnNextAction::Verify,
            },
        )],
    );
    let settled = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &intent_acknowledged,
        &finished,
    ));
    let verified = batch(
        &settled,
        17_110,
        vec![goal_event(
            goal_receipt(
                &goal,
                7,
                1,
                SurfaceGoalReceiptState::Present {
                    state: SurfaceGoalState::Active,
                    current_run: Some(settled_run),
                },
                17_111,
            ),
            GoalPatch::VerificationCompleted {
                identity,
                result: SurfaceGoalVerification::Achieved {
                    evidence: vec![evidence()],
                },
            },
        )],
    );
    let verified_state = applied(reduce_batch(SurfaceReduceMode::Live, &settled, &verified));

    let transition = SurfaceGoalTransition {
        previous: SurfaceGoalState::Active,
        next: SurfaceGoalState::BudgetLimited,
        reason_code: NonEmptyText::try_new("budget").unwrap(),
    };
    let transitioned = batch(
        &verified_state,
        17_112,
        vec![goal_event(
            goal_receipt(
                &goal,
                8,
                1,
                SurfaceGoalReceiptState::Present {
                    state: transition.next.clone(),
                    current_run: None,
                },
                17_113,
            ),
            GoalPatch::Transitioned {
                goal_id: goal.goal_id,
                transition,
            },
        )],
    );
    let transitioned_state = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &verified_state,
        &transitioned,
    ));
    assert!(matches!(
        transitioned_state.snapshot().goal.as_ref().unwrap().state,
        SurfaceGoalState::BudgetLimited
    ));
}

#[test]
fn goal_pause_recover_and_complete_patches_are_receipt_backed() {
    let goal = simple_goal();
    let paused_state = SurfaceGoalState::Paused {
        reason: SurfaceGoalPauseReason::User,
        message: DisplayText::new("paused"),
    };
    let mut pause_snapshot = snapshot();
    pause_snapshot.goal = Some(goal.clone());
    let pause_initial = SurfaceReducerState::new(pause_snapshot);
    let paused = batch(
        &pause_initial,
        17_200,
        vec![goal_event(
            goal_receipt(
                &goal,
                2,
                1,
                SurfaceGoalReceiptState::Present {
                    state: paused_state.clone(),
                    current_run: None,
                },
                17_201,
            ),
            GoalPatch::Paused {
                goal_id: goal.goal_id.clone(),
                goal_run_id: None,
                outer_turn_id: None,
                state: paused_state,
            },
        )],
    );
    applied(reduce_batch(
        SurfaceReduceMode::Live,
        &pause_initial,
        &paused,
    ));

    let identity = goal_identity(0, 1);
    let active_run = SurfaceGoalRun {
        goal_run_id: identity.goal_run_id,
        run_origin: SurfaceGoalRunOrigin::User,
        operation_id: identity.operation_fence.operation_id,
        phase: SurfaceGoalRunPhase::InFlight {
            outer_turn: SurfaceGoalOuterTurnReceipt {
                outer_turn_id: identity.goal_outer_turn_id,
                origin: SurfaceGoalOuterTurnReceiptOrigin::User,
                outer_turn_count: 1,
            },
        },
    };
    let mut recovery_goal = goal.clone();
    recovery_goal.current_run = Some(active_run.clone());
    let mut recovery_snapshot = snapshot();
    recovery_snapshot.goal = Some(recovery_goal.clone());
    let recovery_initial = SurfaceReducerState::new(recovery_snapshot);
    let recovery_commit = SurfaceCommitId::try_from_bytes(uuid_v7_bytes(17_202)).unwrap();
    let recovery_message = DisplayText::new("owner recovered");
    let mut recovery_receipt = goal_receipt(
        &recovery_goal,
        2,
        1,
        SurfaceGoalReceiptState::Present {
            state: SurfaceGoalState::Paused {
                reason: SurfaceGoalPauseReason::Recovery,
                message: recovery_message.clone(),
            },
            current_run: None,
        },
        17_202,
    );
    recovery_receipt.store_commit_id = recovery_commit.clone();
    let recovered = batch(
        &recovery_initial,
        17_203,
        vec![goal_event(
            recovery_receipt,
            GoalPatch::Recovered {
                goal_id: goal.goal_id.clone(),
                stale_run: SurfaceGoalClosedRunReceipt {
                    run: active_run,
                    close_reason: SurfaceGoalCloseReason::Recovery,
                    store_commit_id: recovery_commit,
                    receipt_digest: digest(104),
                },
                recovery_message,
                discarded_continuation: DiscardedContinuation::new(),
            },
        )],
    );
    applied(reduce_batch(
        SurfaceReduceMode::Live,
        &recovery_initial,
        &recovered,
    ));

    let mut complete_snapshot = snapshot();
    complete_snapshot.goal = Some(goal.clone());
    let complete_initial = SurfaceReducerState::new(complete_snapshot);
    let completed_state = SurfaceGoalState::Complete {
        evidence: vec![evidence()],
    };
    let completed = batch(
        &complete_initial,
        17_204,
        vec![goal_event(
            goal_receipt(
                &goal,
                2,
                1,
                SurfaceGoalReceiptState::Present {
                    state: completed_state,
                    current_run: None,
                },
                17_205,
            ),
            GoalPatch::Completed {
                goal_id: goal.goal_id,
                goal_run_id: None,
                evidence: vec![evidence()],
                usage: goal_usage(),
            },
        )],
    );
    let complete = applied(reduce_batch(
        SurfaceReduceMode::Live,
        &complete_initial,
        &completed,
    ));
    assert!(matches!(
        complete.snapshot().goal.as_ref().unwrap().state,
        SurfaceGoalState::Complete { .. }
    ));
}

fn continuation_envelope(
    predecessor: &SurfaceGoalGenerationIdentity,
    successor: &SurfaceGoalGenerationIdentity,
) -> GoalPatchEnvelope {
    let current_run = SurfaceGoalRun {
        goal_run_id: successor.goal_run_id.clone(),
        run_origin: SurfaceGoalRunOrigin::User,
        operation_id: successor.operation_fence.operation_id.clone(),
        phase: SurfaceGoalRunPhase::InFlight {
            outer_turn: SurfaceGoalOuterTurnReceipt {
                outer_turn_id: successor.goal_outer_turn_id.clone(),
                origin: SurfaceGoalOuterTurnReceiptOrigin::Continuation,
                outer_turn_count: successor.outer_turn_count,
            },
        },
    };
    GoalPatchEnvelope {
        receipt: SurfaceGoalStoreReceipt {
            goal_id: predecessor.goal_id.clone(),
            goal_revision: GoalRevision::try_new(3).unwrap(),
            objective_revision: GoalObjectiveRevision::new(1),
            catalog_revision: GoalCatalogRevision::try_new(1).unwrap(),
            goal_owner_epoch: GoalOwnerEpoch::try_new(1).unwrap(),
            row_state: SurfaceGoalReceiptState::Present {
                state: SurfaceGoalState::Active,
                current_run: Some(current_run),
            },
            store_commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(12_010)).unwrap(),
            receipt_digest: digest(80),
        },
        patch: GoalPatch::ContinuationDecided {
            goal_id: predecessor.goal_id.clone(),
            predecessor: predecessor.clone(),
            decision: GoalContinuationDecision::Admitted {
                reason: GoalContinuationAdmitReason::Progress,
                successor: successor.clone(),
            },
        },
    }
}

fn goal_continuation_state(predecessor: &SurfaceGoalGenerationIdentity) -> SurfaceReducerState {
    let mut operation = operation_record();
    operation.operation_id = predecessor.operation_fence.operation_id.clone();
    operation.reservation = reservation_lease(&operation.operation_id);
    operation.intent.kind = OperationKind::GoalRun {
        goal_id: predecessor.goal_id.clone(),
        goal_run_id: predecessor.goal_run_id.clone(),
        initial_objective_revision: predecessor.objective_revision,
    };
    operation.phase = OperationPhase::Admitted;
    operation.initial_logical_turn_id = Some(predecessor.logical_turn_id.clone());
    operation.initial_input_item_id = Some(predecessor.canonical_input_item_id.clone());
    let fact = SurfaceResolvedInputFact::NonReplayable {
        presentation: SurfaceInputPresentation::Redacted,
        live_capsule_incarnation: incarnation(),
    };
    let mut generation = generation(&operation);
    generation.fence = predecessor.operation_fence.clone();
    generation.logical_turn_id = predecessor.logical_turn_id.clone();
    generation.input = GenerationInputState::Resolved {
        input_item_id: predecessor.canonical_input_item_id.clone(),
        fact: fact.clone(),
    };
    generation.goal_identity = Some(predecessor.clone());
    generation.phase = GenerationPhase::Started;
    generation.started_witness = Some(generation_started_witness(
        12_020,
        &generation.replayability,
    ));
    operation.generations.push(generation);

    let mut initial = snapshot();
    initial.goal = Some(goal_with_predecessor(predecessor));
    initial.foreground_operation = Some(operation);
    initial.items.push(SurfaceItem::UserMessage {
        id: predecessor.canonical_input_item_id.clone(),
        turn_id: predecessor.logical_turn_id.clone(),
        input: SurfaceUserInputState::Resolved { fact },
        pinned: false,
        origin: SurfaceItemOrigin::UserInput,
    });
    SurfaceReducerState::new(initial)
}

fn successor_generation_and_item(
    state: &SurfaceReducerState,
    successor: &SurfaceGoalGenerationIdentity,
) -> (GenerationRecord, SurfaceItem) {
    let operation = state.snapshot().foreground_operation.as_ref().unwrap();
    let presentation = SurfaceInputPresentation::Visible {
        text: DisplayText::new("continue goal"),
    };
    let correlation_id = SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(12_021)).unwrap();
    let generation = GenerationRecord {
        fence: successor.operation_fence.clone(),
        logical_turn_id: successor.logical_turn_id.clone(),
        input: GenerationInputState::Pending {
            input_item_id: successor.canonical_input_item_id.clone(),
            presentation: presentation.clone(),
            correlation_id: correlation_id.clone(),
        },
        predecessor: successor.predecessor_fence.clone(),
        attempt: successor.attempt,
        goal_identity: Some(successor.clone()),
        replayability: operation.intent.initial_replayability.clone(),
        required_capabilities: operation.intent.required_capabilities.clone(),
        capability_fingerprint: operation.intent.capability_fingerprint.clone(),
        phase: GenerationPhase::Reserved,
        started_witness: None,
        stop_reason: None,
    };
    let item = SurfaceItem::UserMessage {
        id: successor.canonical_input_item_id.clone(),
        turn_id: successor.logical_turn_id.clone(),
        input: SurfaceUserInputState::Pending {
            presentation,
            correlation_id,
        },
        pinned: false,
        origin: SurfaceItemOrigin::GoalContinuation,
    };
    (generation, item)
}

fn complete_goal_continuation_batch(
    state: &SurfaceReducerState,
    seed: u32,
    predecessor: &SurfaceGoalGenerationIdentity,
    successor: &SurfaceGoalGenerationIdentity,
) -> SurfaceCommitBatch {
    let goal = state.snapshot().goal.as_ref().unwrap();
    let mut settled_run = goal.current_run.as_ref().unwrap().clone();
    settled_run.phase = SurfaceGoalRunPhase::Settled {
        last_outer_turn: Some(SurfaceGoalOuterTurnReceipt {
            outer_turn_id: predecessor.goal_outer_turn_id.clone(),
            origin: SurfaceGoalOuterTurnReceiptOrigin::User,
            outer_turn_count: predecessor.outer_turn_count,
        }),
    };
    let outer_finished = GoalPatchEnvelope {
        receipt: goal_receipt(
            goal,
            2,
            1,
            SurfaceGoalReceiptState::Present {
                state: SurfaceGoalState::Active,
                current_run: Some(settled_run),
            },
            seed + 10,
        ),
        patch: GoalPatch::OuterTurnFinished {
            identity: predecessor.clone(),
            status: GoalOuterTurnStatus::Success,
            usage: goal_usage(),
            next_action: GoalOuterTurnNextAction::Continue,
        },
    };
    let (successor_generation, successor_item) = successor_generation_and_item(state, successor);
    batch(
        state,
        seed,
        vec![
            (
                SurfaceScope::Generation {
                    fence: predecessor.operation_fence.clone(),
                },
                SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                    fence: predecessor.operation_fence.clone(),
                    reason: GenerationStopReason::Completed {
                        status: GenerationCompletionStatus::Success,
                    },
                    usage_delta: vector_usage(),
                }),
            ),
            (
                SurfaceScope::Goal {
                    goal_id: predecessor.goal_id.clone(),
                    causative_generation: Some(predecessor.operation_fence.clone()),
                },
                SurfaceEvent::Goal(outer_finished),
            ),
            (
                SurfaceScope::Goal {
                    goal_id: predecessor.goal_id.clone(),
                    causative_generation: Some(predecessor.operation_fence.clone()),
                },
                SurfaceEvent::Goal(continuation_envelope(predecessor, successor)),
            ),
            (
                SurfaceScope::Generation {
                    fence: successor.operation_fence.clone(),
                },
                SurfaceEvent::Operation(OperationPatch::GenerationReserved {
                    generation: successor_generation,
                }),
            ),
            (
                SurfaceScope::Generation {
                    fence: successor.operation_fence.clone(),
                },
                SurfaceEvent::Item(ItemPatch::Added {
                    item: successor_item,
                }),
            ),
        ],
    )
}

fn complete_goal_stop_batch(
    state: &SurfaceReducerState,
    seed: u32,
    predecessor: &SurfaceGoalGenerationIdentity,
    reason: GoalContinuationStopReason,
    goal_state: SurfaceGoalState,
    terminal: OperationTerminal,
) -> SurfaceCommitBatch {
    let goal = state.snapshot().goal.as_ref().unwrap();
    let mut settled_run = goal.current_run.as_ref().unwrap().clone();
    settled_run.phase = SurfaceGoalRunPhase::Settled {
        last_outer_turn: Some(SurfaceGoalOuterTurnReceipt {
            outer_turn_id: predecessor.goal_outer_turn_id.clone(),
            origin: SurfaceGoalOuterTurnReceiptOrigin::User,
            outer_turn_count: predecessor.outer_turn_count,
        }),
    };
    let stop_reason = match &reason {
        GoalContinuationStopReason::BudgetLimited { budget } => GenerationStopReason::Completed {
            status: GenerationCompletionStatus::BudgetExhausted {
                budget: budget.as_budget().clone(),
            },
        },
        GoalContinuationStopReason::RuntimeFailure { class, message } => {
            GenerationStopReason::ExecutionFailed {
                class: match class {
                    FailureClass::Provider => GenerationExecutionFailureClass::Provider,
                    FailureClass::Tool => GenerationExecutionFailureClass::Tool,
                    FailureClass::Hook => GenerationExecutionFailureClass::Hook,
                    FailureClass::Workflow => GenerationExecutionFailureClass::Workflow,
                    FailureClass::Verification => GenerationExecutionFailureClass::RuntimeInvariant,
                    FailureClass::InputResolution => {
                        GenerationExecutionFailureClass::InputResolution
                    }
                    FailureClass::ClientCapabilityUnavailable => {
                        GenerationExecutionFailureClass::ClientCapabilityUnavailable
                    }
                    FailureClass::LegacyApprovalRequired => {
                        GenerationExecutionFailureClass::LegacyApprovalRequired
                    }
                    FailureClass::RuntimeInvariant => {
                        GenerationExecutionFailureClass::RuntimeInvariant
                    }
                    FailureClass::Persistence => GenerationExecutionFailureClass::RuntimeInvariant,
                    FailureClass::ExternalEffectAmbiguous => {
                        GenerationExecutionFailureClass::ExternalEffectAmbiguous
                    }
                    FailureClass::RemoteResourceCleanupAmbiguous => {
                        GenerationExecutionFailureClass::RemoteResourceCleanupAmbiguous
                    }
                },
                message: message.clone(),
            }
        }
        _ => GenerationStopReason::Completed {
            status: GenerationCompletionStatus::Success,
        },
    };
    let outer_status = if matches!(&reason, GoalContinuationStopReason::RuntimeFailure { .. }) {
        GoalOuterTurnStatus::Failed
    } else if matches!(&reason, GoalContinuationStopReason::BudgetLimited { .. }) {
        GoalOuterTurnStatus::BudgetExhausted
    } else {
        GoalOuterTurnStatus::Success
    };
    batch(
        state,
        seed,
        vec![
            (
                SurfaceScope::Generation {
                    fence: predecessor.operation_fence.clone(),
                },
                SurfaceEvent::Operation(OperationPatch::GenerationStopped {
                    fence: predecessor.operation_fence.clone(),
                    reason: stop_reason.clone(),
                    usage_delta: vector_usage(),
                }),
            ),
            goal_event(
                goal_receipt(
                    goal,
                    2,
                    1,
                    SurfaceGoalReceiptState::Present {
                        state: SurfaceGoalState::Active,
                        current_run: Some(settled_run),
                    },
                    seed + 1,
                ),
                GoalPatch::OuterTurnFinished {
                    identity: predecessor.clone(),
                    status: outer_status,
                    usage: goal_usage(),
                    next_action: GoalOuterTurnNextAction::Pause,
                },
            ),
            goal_event(
                goal_receipt(
                    goal,
                    3,
                    1,
                    SurfaceGoalReceiptState::Present {
                        state: goal_state.clone(),
                        current_run: None,
                    },
                    seed + 2,
                ),
                GoalPatch::ContinuationDecided {
                    goal_id: predecessor.goal_id.clone(),
                    predecessor: predecessor.clone(),
                    decision: GoalContinuationDecision::Stopped {
                        reason,
                        outer_turn_count: predecessor.outer_turn_count,
                        goal_state,
                        terminal,
                    },
                },
            ),
            (
                SurfaceScope::Operation {
                    operation_id: predecessor.operation_fence.operation_id.clone(),
                },
                SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                    operation_id: predecessor.operation_fence.operation_id.clone(),
                    finalize_intent_id: SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(
                        seed + 3,
                    ))
                    .unwrap(),
                    terminal_commit_id: recorded_commit_id(seed),
                    selected_cause: OperationFinalizationCause::GenerationStop(stop_reason),
                    suspended_cause: None,
                    expected_settlements: Vec::new(),
                }),
            ),
        ],
    )
}

#[test]
fn goal_stopped_decision_is_atomic_with_finalization() {
    let predecessor = goal_identity(0, 1);
    let initial = goal_continuation_state(&predecessor);
    let goal_state = SurfaceGoalState::Paused {
        reason: SurfaceGoalPauseReason::NoProgress,
        message: DisplayText::new("plan mode"),
    };
    let terminal = OperationTerminal::Succeeded {
        usage: vector_usage(),
    };
    let complete = complete_goal_stop_batch(
        &initial,
        12_150,
        &predecessor,
        GoalContinuationStopReason::PlanModeDisallowsContinuation,
        goal_state,
        terminal,
    );
    let applied_state = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &complete));
    assert!(matches!(
        applied_state
            .snapshot()
            .foreground_operation
            .as_ref()
            .unwrap()
            .phase,
        OperationPhase::Finalizing { .. }
    ));

    let mut events = complete.events.as_slice()[..3].to_vec();
    for (ordinal, event) in events.iter_mut().enumerate() {
        event.ordinal = ordinal as u32;
    }
    let mut partial = SurfaceCommitBatch {
        cursor_before: complete.cursor_before,
        cursor_after: SurfaceCursor {
            next_seq: SequenceNumber::new(3),
            ..complete.cursor_after
        },
        commit_class: complete.commit_class,
        event_count: 3,
        batch_digest: digest(0),
        events: NonEmptyVec::try_new(events).unwrap(),
    };
    partial.batch_digest = canonical_batch_digest(&partial);
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &partial),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
}

#[test]
fn goal_stopped_decision_requires_closed_reason_state_terminal_mapping() {
    let predecessor = goal_identity(0, 1);
    let initial = goal_continuation_state(&predecessor);
    let invalid = complete_goal_stop_batch(
        &initial,
        12_160,
        &predecessor,
        GoalContinuationStopReason::PlanModeDisallowsContinuation,
        SurfaceGoalState::Paused {
            reason: SurfaceGoalPauseReason::User,
            message: DisplayText::new("wrong pause class"),
        },
        OperationTerminal::Succeeded {
            usage: vector_usage(),
        },
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::GoalReceiptMismatch,
    );
}

#[test]
fn goal_inactive_stop_rejects_the_active_state_complement() {
    let predecessor = goal_identity(0, 1);
    let initial = goal_continuation_state(&predecessor);
    let invalid = complete_goal_stop_batch(
        &initial,
        12_161,
        &predecessor,
        GoalContinuationStopReason::GoalInactive {
            state: SurfaceGoalState::Active,
        },
        SurfaceGoalState::Active,
        OperationTerminal::Succeeded {
            usage: vector_usage(),
        },
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::GoalReceiptMismatch,
    );
}

#[test]
fn goal_runtime_failure_stop_uses_the_explicit_failure_mapping() {
    let predecessor = goal_identity(0, 1);
    let initial = goal_continuation_state(&predecessor);
    let message = safe_message("provider failed");
    let complete = complete_goal_stop_batch(
        &initial,
        12_170,
        &predecessor,
        GoalContinuationStopReason::RuntimeFailure {
            class: FailureClass::Provider,
            message: message.clone(),
        },
        SurfaceGoalState::Paused {
            reason: SurfaceGoalPauseReason::Infrastructure,
            message: DisplayText::new("provider failed"),
        },
        OperationTerminal::Failed {
            class: FailureClass::Provider,
            message,
        },
    );

    applied(reduce_batch(SurfaceReduceMode::Live, &initial, &complete));
}

#[test]
fn goal_budget_stop_requires_the_exact_goal_budget_terminal() {
    let predecessor = goal_identity(0, 1);
    let initial = goal_continuation_state(&predecessor);
    let budget = GoalTokenBudget::try_new(OperationBudget::GoalTokenBudget {
        goal_id: predecessor.goal_id.clone(),
        limit: 100,
        observed: 100,
    })
    .unwrap();
    let terminal_budget = budget.as_budget().clone();
    let complete = complete_goal_stop_batch(
        &initial,
        12_180,
        &predecessor,
        GoalContinuationStopReason::BudgetLimited { budget },
        SurfaceGoalState::BudgetLimited,
        OperationTerminal::BudgetExhausted {
            budget: terminal_budget,
        },
    );

    applied(reduce_batch(SurfaceReduceMode::Live, &initial, &complete));
}

#[test]
fn goal_continuation_replay_is_batch_exact_and_receipt_must_match() {
    let predecessor = goal_identity(0, 1);
    let successor = goal_identity(1, 2);
    let envelope = continuation_envelope(&predecessor, &successor);
    let initial = goal_continuation_state(&predecessor);
    let first = complete_goal_continuation_batch(&initial, 12_100, &predecessor, &successor);
    let applied_state = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &first));
    assert_eq!(
        applied_state
            .snapshot()
            .goal
            .as_ref()
            .unwrap()
            .goal_revision
            .get(),
        3
    );
    assert_eq!(
        applied_state
            .snapshot()
            .goal
            .as_ref()
            .unwrap()
            .receipt_digest,
        digest(80)
    );

    let first_commit_id = match &first.commit_class {
        CommitClass::Recorded { commit_id, .. } | CommitClass::Ephemeral { commit_id, .. } => {
            commit_id.clone()
        }
    };
    assert!(matches!(
        reduce_batch(SurfaceReduceMode::Rematerialization, &applied_state, &first),
        SurfaceReduceResult::AlreadyApplied { cursor, commit_id }
            if cursor == first.cursor_after && commit_id == first_commit_id
    ));

    let same_envelope_new_batch = batch(
        &applied_state,
        12_101,
        vec![(
            SurfaceScope::Goal {
                goal_id: predecessor.goal_id.clone(),
                causative_generation: Some(predecessor.operation_fence.clone()),
            },
            SurfaceEvent::Goal(envelope.clone()),
        )],
    );
    assert_ne!(same_envelope_new_batch.commit_class, first.commit_class);
    assert_ne!(same_envelope_new_batch.cursor_before, first.cursor_before);
    assert_ne!(
        same_envelope_new_batch.events.as_slice()[0].event_id,
        first.events.as_slice()[0].event_id
    );
    rejected(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &same_envelope_new_batch,
        ),
        SurfaceReducerErrorCode::StaleRevision,
    );

    let changed_successor = goal_identity(1, 3);
    let conflict = continuation_envelope(&predecessor, &changed_successor);
    let conflict_batch = batch(
        &applied_state,
        12_102,
        vec![(
            SurfaceScope::Goal {
                goal_id: predecessor.goal_id.clone(),
                causative_generation: Some(predecessor.operation_fence.clone()),
            },
            SurfaceEvent::Goal(conflict),
        )],
    );
    rejected(
        reduce_batch(
            SurfaceReduceMode::Rematerialization,
            &applied_state,
            &conflict_batch,
        ),
        SurfaceReducerErrorCode::StaleRevision,
    );

    let mut mismatched_batch =
        complete_goal_continuation_batch(&initial, 12_103, &predecessor, &successor);
    let mut events = mismatched_batch.events.as_slice().to_vec();
    let SurfaceEvent::Goal(mismatched) = &mut events[2].event else {
        unreachable!()
    };
    mismatched.receipt.row_state = SurfaceGoalReceiptState::Present {
        state: SurfaceGoalState::BudgetLimited,
        current_run: None,
    };
    mismatched_batch.events = NonEmptyVec::try_new(events).unwrap();
    mismatched_batch.batch_digest = canonical_batch_digest(&mismatched_batch);
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &mismatched_batch),
        SurfaceReducerErrorCode::GoalReceiptMismatch,
    );
}

#[test]
fn cursor_after_carries_the_commit_source_revision() {
    let initial = state();
    let mut advancing = batch(
        &initial,
        12_200,
        vec![(SurfaceScope::Thread, plan_event(2, "revision advanced"))],
    );
    let next_revision = DurableRevision::try_new(2).unwrap();
    advancing.cursor_after.source_revision = CursorSourceRevision::Recorded {
        durable_revision: next_revision,
    };
    let commit_id = recorded_commit_id(12_200);
    advancing.commit_class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        durable_revision: next_revision,
        commit_id,
    };
    let mut events = advancing.events.as_slice().to_vec();
    events[0].commit_class = advancing.commit_class.clone();
    advancing.events = NonEmptyVec::try_new(events).unwrap();
    advancing.batch_digest = canonical_batch_digest(&advancing);

    let applied = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &advancing));
    assert_eq!(applied.snapshot().cursor, advancing.cursor_after);
}

#[test]
fn event_id_cannot_be_reused_by_a_different_commit() {
    let initial = state();
    let first = batch(
        &initial,
        12_210,
        vec![(SurfaceScope::Thread, plan_event(2, "first"))],
    );
    let after_first = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &first));
    let mut second = batch(
        &after_first,
        12_211,
        vec![(SurfaceScope::Thread, plan_event(3, "second"))],
    );
    let mut events = second.events.as_slice().to_vec();
    events[0].event_id = first.events.as_slice()[0].event_id.clone();
    second.events = NonEmptyVec::try_new(events).unwrap();
    second.batch_digest = canonical_batch_digest(&second);

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &after_first, &second),
        SurfaceReducerErrorCode::DuplicateTransition,
    );
}

#[test]
fn background_scope_requires_a_matching_runtime_background_summary() {
    let operation = operation_record();
    let initial = operation_state(operation.clone(), true);
    let terminal_seed = 12_220;
    let forged_background = background_fence(&generation(&operation).fence, 99);
    let invalid = batch(
        &initial,
        terminal_seed,
        vec![(
            SurfaceScope::Background {
                fence: forged_background,
            },
            SurfaceEvent::Operation(OperationPatch::FinalizationStarted {
                operation_id: operation.operation_id,
                finalize_intent_id: SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(
                    terminal_seed + 1,
                ))
                .unwrap(),
                terminal_commit_id: recorded_commit_id(terminal_seed),
                selected_cause: OperationFinalizationCause::Reservation(
                    ReservationFinalizerReason::ReservationExpired,
                ),
                suspended_cause: None,
                expected_settlements: Vec::new(),
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::ScopeMismatch,
    );
}

#[test]
fn session_closed_requires_the_exact_closing_witness() {
    let initial = state();
    let barrier_id = SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(12_230)).unwrap();
    let closing_commit_id = recorded_commit_id(12_231);
    let closing = batch(
        &initial,
        12_232,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Session(SessionPatch::Closing {
                reason: SurfaceShutdownReason::ThreadClose,
                barrier_id: barrier_id.clone(),
                closing_commit_id: closing_commit_id.clone(),
                plan_digest: digest(81),
            }),
        )],
    );
    let closing_state = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &closing));
    let mismatched = batch(
        &closing_state,
        12_233,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Session(SessionPatch::Closed {
                reason: SurfaceShutdownReason::HostShutdown,
                barrier_id,
                closing_commit_id,
                plan_digest: digest(81),
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &closing_state, &mismatched),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn task_reconciliation_requires_unique_identities_and_revision_one_for_new_tasks() {
    let initial = state();
    let canonical = task(SurfaceTaskStatus::Running, 1);
    let duplicate = batch(
        &initial,
        12_240,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::Reconciled {
                source_revision: TaskRevision::try_new(1).unwrap(),
                tasks: vec![canonical.clone(), canonical],
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &duplicate),
        SurfaceReducerErrorCode::DuplicateTransition,
    );

    let non_initial = batch(
        &initial,
        12_241,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::Reconciled {
                source_revision: TaskRevision::try_new(2).unwrap(),
                tasks: vec![task(SurfaceTaskStatus::Running, 2)],
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &non_initial),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn task_reconciliation_cannot_replace_a_task_identity() {
    let current = task(SurfaceTaskStatus::Running, 1);
    let mut initial_snapshot = snapshot();
    initial_snapshot.tasks.push(current.clone());
    let initial = SurfaceReducerState::new(initial_snapshot);
    let mut replacement = current;
    replacement.revision = TaskRevision::try_new(2).unwrap();
    replacement.task_type = SurfaceTaskType::Subagent;
    replacement.status = SurfaceTaskStatus::Completed;
    let invalid = batch(
        &initial,
        12_242,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::Reconciled {
                source_revision: TaskRevision::try_new(2).unwrap(),
                tasks: vec![replacement],
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn task_reconciliation_accepts_constrained_terminal_history() {
    for (seed, status) in [
        (12_243, SurfaceTaskStatus::Completed),
        (12_244, SurfaceTaskStatus::Stopped),
        (12_245, SurfaceTaskStatus::Cancelled),
    ] {
        let initial = state();
        let mut historical = task(status, 1);
        historical.completed_at = Some(UnixMillis::new(3));
        historical.result = Some(DisplayText::new("legacy terminal history"));
        let reconciled = batch(
            &initial,
            seed,
            vec![(
                SurfaceScope::Thread,
                SurfaceEvent::Task(TaskPatch::Reconciled {
                    source_revision: TaskRevision::try_new(1).unwrap(),
                    tasks: vec![historical.clone()],
                }),
            )],
        );

        let next = applied(reduce_batch(SurfaceReduceMode::Live, &initial, &reconciled));
        assert!(next.snapshot().tasks == vec![historical]);
    }
}

#[test]
fn task_reconciliation_cannot_omit_or_change_current_tasks() {
    let current = task(SurfaceTaskStatus::Running, 1);
    let mut initial_snapshot = snapshot();
    initial_snapshot.tasks.push(current.clone());
    let initial = SurfaceReducerState::new(initial_snapshot);
    let omitted = batch(
        &initial,
        12_246,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::Reconciled {
                source_revision: TaskRevision::try_new(1).unwrap(),
                tasks: Vec::new(),
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &omitted),
        SurfaceReducerErrorCode::IllegalTransition,
    );

    let mut changed = current;
    changed.description = DisplayText::new("changed identity");
    let changed = batch(
        &initial,
        12_247,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::Reconciled {
                source_revision: TaskRevision::try_new(1).unwrap(),
                tasks: vec![changed],
            }),
        )],
    );
    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &changed),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn task_reconciliation_rejects_actionable_terminal_history() {
    let mut candidates = Vec::new();

    let mut backgrounded = task(SurfaceTaskStatus::Completed, 1);
    backgrounded.backgrounded = true;
    candidates.push(backgrounded);

    let mut operation_owned = task(SurfaceTaskStatus::Stopped, 1);
    operation_owned.parent_operation = Some(operation_fence().operation_id);
    candidates.push(operation_owned);

    let mut workflow_owned = task(SurfaceTaskStatus::Cancelled, 1);
    workflow_owned.workflow_run_id = Some(SurfaceWorkflowRunId::try_new("legacy-run").unwrap());
    candidates.push(workflow_owned);

    let mut subagent_owned = task(SurfaceTaskStatus::Completed, 1);
    subagent_owned.subagent_id = Some(SurfaceSubagentId::try_new("legacy-agent").unwrap());
    candidates.push(subagent_owned);

    let mut interaction_owned = task(SurfaceTaskStatus::Stopped, 1);
    interaction_owned.pending_interaction_id =
        Some(SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(12_248)).unwrap());
    candidates.push(interaction_owned);

    for (offset, candidate) in candidates.into_iter().enumerate() {
        let initial = state();
        let invalid = batch(
            &initial,
            12_249 + u32::try_from(offset).unwrap(),
            vec![(
                SurfaceScope::Thread,
                SurfaceEvent::Task(TaskPatch::Reconciled {
                    source_revision: TaskRevision::try_new(1).unwrap(),
                    tasks: vec![candidate],
                }),
            )],
        );
        rejected(
            reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
            SurfaceReducerErrorCode::IllegalTransition,
        );
    }
}

#[test]
fn workflow_agent_patch_discriminant_must_match_the_target_state() {
    let mut initial_snapshot = snapshot();
    let mut running_workflow = workflow(SurfaceWorkflowStatus::Running);
    running_workflow
        .agents
        .push(workflow_agent(SurfaceWorkflowAgentStatus::Pending));
    initial_snapshot.workflows.push(running_workflow);
    let initial = SurfaceReducerState::new(initial_snapshot);
    let invalid = batch(
        &initial,
        12_250,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Workflow(WorkflowPatch::AgentStarted {
                fence: SurfaceWorkflowFence {
                    workflow_run_id: SurfaceWorkflowRunId::try_new("manifest-workflow").unwrap(),
                    workflow_revision: WorkflowRevision::try_new(1).unwrap(),
                    parent: None,
                },
                next_revision: WorkflowRevision::try_new(2).unwrap(),
                agent: workflow_agent(SurfaceWorkflowAgentStatus::Cached),
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn workflow_agent_transition_cannot_replace_the_attempt_identity() {
    let mut initial_snapshot = snapshot();
    let mut running_workflow = workflow(SurfaceWorkflowStatus::Running);
    running_workflow
        .agents
        .push(workflow_agent(SurfaceWorkflowAgentStatus::Pending));
    initial_snapshot.workflows.push(running_workflow);
    let initial = SurfaceReducerState::new(initial_snapshot);
    let mut forged_agent = workflow_agent(SurfaceWorkflowAgentStatus::Running);
    forged_agent.phase = NonEmptyText::try_new("forged-phase").unwrap();
    let invalid = batch(
        &initial,
        12_251,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Workflow(WorkflowPatch::AgentStarted {
                fence: SurfaceWorkflowFence {
                    workflow_run_id: SurfaceWorkflowRunId::try_new("manifest-workflow").unwrap(),
                    workflow_revision: WorkflowRevision::try_new(1).unwrap(),
                    parent: None,
                },
                next_revision: WorkflowRevision::try_new(2).unwrap(),
                agent: forged_agent,
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::IllegalTransition,
    );
}

#[test]
fn revision_overflow_is_not_a_contiguous_transition() {
    let mut initial_snapshot = snapshot();
    initial_snapshot
        .tasks
        .push(task(SurfaceTaskStatus::Running, u64::MAX));
    let initial = SurfaceReducerState::new(initial_snapshot);
    let task_id = initial.snapshot().tasks[0].task_id.clone();
    let invalid = batch(
        &initial,
        12_260,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Task(TaskPatch::StatusChanged {
                task_id,
                expected_revision: TaskRevision::try_new(u64::MAX).unwrap(),
                next_revision: TaskRevision::try_new(u64::MAX).unwrap(),
                status: SurfaceTaskStatus::Paused,
                completed_at: None,
                result: None,
                error: None,
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::StaleRevision,
    );
}

#[test]
fn scalar_projection_revision_overflow_is_rejected_without_panicking() {
    let mut initial_snapshot = snapshot();
    initial_snapshot.plan.revision = PlanRevision::try_new(u64::MAX).unwrap();
    let initial = SurfaceReducerState::new(initial_snapshot);
    let invalid = batch(
        &initial,
        12_201,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Plan(SurfacePlanSnapshot {
                revision: PlanRevision::try_new(u64::MAX).unwrap(),
                explanation: Some(DisplayText::new("overflow")),
                items: Vec::new(),
                causative_generation: None,
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::StaleRevision,
    );
    assert_eq!(initial.snapshot().plan.revision.get(), u64::MAX);
}

#[test]
fn workflow_revision_overflow_is_not_contiguous() {
    let mut workflow = workflow(SurfaceWorkflowStatus::Running);
    workflow.revision = WorkflowRevision::try_new(u64::MAX).unwrap();
    let fence = SurfaceWorkflowFence {
        workflow_run_id: workflow.workflow_run_id.clone(),
        workflow_revision: workflow.revision,
        parent: workflow.parent.clone(),
    };
    let mut initial_snapshot = snapshot();
    initial_snapshot.workflows.push(workflow);
    let initial = SurfaceReducerState::new(initial_snapshot);
    let invalid = batch(
        &initial,
        12_261,
        vec![(
            SurfaceScope::Thread,
            SurfaceEvent::Workflow(WorkflowPatch::Paused {
                fence,
                next_revision: WorkflowRevision::try_new(u64::MAX).unwrap(),
                reason: DisplayText::new("overflow"),
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::StaleRevision,
    );
}

#[test]
fn subagent_revision_overflow_is_not_contiguous() {
    let mut subagent = subagent(SurfaceSubagentStatus::Running, u64::MAX);
    let owner = subagent.owner.clone();
    let subagent_id = subagent.subagent_id.clone();
    subagent.revision = SubagentRevision::try_new(u64::MAX).unwrap();
    let mut initial_snapshot = snapshot();
    initial_snapshot.subagents.push(subagent);
    let initial = SurfaceReducerState::new(initial_snapshot);
    let invalid = batch(
        &initial,
        12_262,
        vec![(
            SurfaceScope::Generation {
                fence: operation_fence(),
            },
            SurfaceEvent::Subagent(SubagentPatch::Progress {
                subagent_id,
                expected_revision: SubagentRevision::try_new(u64::MAX).unwrap(),
                next_revision: SubagentRevision::try_new(u64::MAX).unwrap(),
                owner,
                source: subagent_source(u64::MAX),
                activity: DisplayText::new("overflow"),
                turn: None,
                usage: None,
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::StaleRevision,
    );
}

#[test]
fn goal_continuation_requires_the_complete_atomic_successor_batch() {
    let predecessor = goal_identity(0, 1);
    let successor = goal_identity(1, 2);
    let initial = goal_continuation_state(&predecessor);
    let complete = complete_goal_continuation_batch(&initial, 12_270, &predecessor, &successor);
    let mut events = complete.events.as_slice()[..3].to_vec();
    for (ordinal, event) in events.iter_mut().enumerate() {
        event.ordinal = ordinal as u32;
    }
    let mut partial = SurfaceCommitBatch {
        cursor_before: complete.cursor_before,
        cursor_after: SurfaceCursor {
            next_seq: SequenceNumber::new(3),
            ..complete.cursor_after
        },
        commit_class: complete.commit_class,
        event_count: 3,
        batch_digest: digest(0),
        events: NonEmptyVec::try_new(events).unwrap(),
    };
    partial.batch_digest = canonical_batch_digest(&partial);

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &partial),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
}

#[test]
fn pending_admission_requires_exactly_one_matching_user_item() {
    let operation = operation_record();
    let presentation = SurfaceInputPresentation::Visible {
        text: DisplayText::new("pending admission"),
    };
    let correlation_id = SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(12_280)).unwrap();
    let item_id = SurfaceItemId::new();
    let mut first_generation = generation(&operation);
    first_generation.input = GenerationInputState::Pending {
        input_item_id: item_id.clone(),
        presentation: presentation.clone(),
        correlation_id: correlation_id.clone(),
    };
    let initial = operation_state(operation.clone(), true);
    let invalid = batch(
        &initial,
        12_281,
        vec![(
            operation_scope(&operation),
            SurfaceEvent::Operation(OperationPatch::Admitted {
                operation_id: operation.operation_id,
                logical_turn_id: first_generation.logical_turn_id.clone(),
                input: AdmittedInput::PendingUser {
                    item_id,
                    presentation,
                    correlation_id,
                },
                first_generation,
            }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
}

#[test]
fn item_input_resolution_requires_the_matching_operation_fact() {
    let (active, generation) = active_generation_state();
    let item_id = SurfaceItemId::new();
    let presentation = SurfaceInputPresentation::Visible {
        text: DisplayText::new("resolve me"),
    };
    let correlation_id = SurfaceInputCorrelationId::try_from_bytes(uuid_v7_bytes(12_290)).unwrap();
    let fact = SurfaceResolvedInputFact::NonReplayable {
        presentation: presentation.clone(),
        live_capsule_incarnation: incarnation(),
    };
    let mut snapshot = active.snapshot().clone();
    snapshot.foreground_operation.as_mut().unwrap().generations[0].input =
        GenerationInputState::Pending {
            input_item_id: item_id.clone(),
            presentation: presentation.clone(),
            correlation_id: correlation_id.clone(),
        };
    snapshot.items.push(SurfaceItem::UserMessage {
        id: item_id.clone(),
        turn_id: generation.logical_turn_id.clone(),
        input: SurfaceUserInputState::Pending {
            presentation,
            correlation_id,
        },
        pinned: false,
        origin: SurfaceItemOrigin::UserInput,
    });
    let initial = SurfaceReducerState::new(snapshot);
    let invalid = batch(
        &initial,
        12_291,
        vec![(
            generation_scope(&generation),
            SurfaceEvent::Item(ItemPatch::InputResolved { item_id, fact }),
        )],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
}

#[test]
fn tool_completion_requires_exactly_one_matching_result_item() {
    let (active, generation) = active_generation_state();
    let tool_call_id = SurfaceToolCallId::try_new("duplicate-result-item").unwrap();
    let request = SurfaceToolRequest {
        tool_call_id: tool_call_id.clone(),
        source_response_id: None,
        turn_id: generation.logical_turn_id.clone(),
        name: NonEmptyText::try_new("bash").unwrap(),
        action: SurfaceToolAction::Shell,
        target: None,
        raw_arguments: DisplayText::new("{}"),
        arguments_digest: digest(82),
    };
    let terminal = SurfaceToolTerminal {
        kind: SurfaceToolResultKind::Success,
        source: ToolTerminalSource::Observed,
        invocation_started: ToolInvocationStarted::Yes,
    };
    let result = SurfaceToolResult {
        tool_call_id: tool_call_id.clone(),
        name: request.name.clone(),
        terminal: terminal.clone(),
        output: Some(DisplayText::new("ok")),
        error: None,
        exit_code: Some(0),
        truncated: false,
        file_change: None,
    };
    let mut snapshot = active.snapshot().clone();
    snapshot.tools.push(SurfaceToolView {
        request,
        state: SurfaceToolViewState::Running,
        invocation_started: None,
        arguments_bytes: ByteCount::new(2),
        output_bytes: ByteCount::new(0),
        streamed_output: DisplayText::new(""),
        streamed_output_truncated: false,
        result: None,
        capability_calls: Vec::new(),
        terminal_leases: Vec::new(),
    });
    let initial = SurfaceReducerState::new(snapshot);
    let result_item = || SurfaceItem::ToolResultMessage {
        id: SurfaceItemId::new(),
        turn_id: generation.logical_turn_id.clone(),
        tool_call_id: tool_call_id.clone(),
        content: DisplayText::new("ok"),
        terminal: terminal.clone(),
        pinned: false,
    };
    let invalid = batch(
        &initial,
        12_300,
        vec![
            (
                generation_scope(&generation),
                SurfaceEvent::Tool(ToolPatch::Completed { result }),
            ),
            (
                generation_scope(&generation),
                SurfaceEvent::Item(ItemPatch::Added {
                    item: result_item(),
                }),
            ),
            (
                generation_scope(&generation),
                SurfaceEvent::Item(ItemPatch::Added {
                    item: result_item(),
                }),
            ),
        ],
    );

    rejected(
        reduce_batch(SurfaceReduceMode::Live, &initial, &invalid),
        SurfaceReducerErrorCode::InvalidOrdering,
    );
}
