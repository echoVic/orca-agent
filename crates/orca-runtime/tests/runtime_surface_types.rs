use orca_runtime::surface::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

fn uuid_v7_bytes(seed: u8) -> [u8; 16] {
    let mut bytes = [seed; 16];
    bytes[6] = 0x70 | (seed & 0x0f);
    bytes[8] = 0x80 | (seed & 0x3f);
    bytes
}

fn operation_fence(seed: u8) -> SurfaceOperationFence {
    SurfaceOperationFence {
        thread_id: SurfaceThreadId::try_from_bytes([seed; 16]).unwrap(),
        thread_owner_epoch: ThreadOwnerEpoch::new(0),
        operation_id: SurfaceOperationId::try_from_bytes(uuid_v7_bytes(seed)).unwrap(),
        generation_id: SurfaceGenerationId::new(0),
    }
}

#[test]
fn primitive_wrappers_enforce_the_frozen_contract() {
    assert!(NonEmptyText::try_new("surface").is_ok());
    assert!(NonEmptyText::try_new(" \n\t").is_err());

    assert!(SafeDiagnosticText::try_new("x".repeat(4_096)).is_ok());
    assert!(SafeDiagnosticText::try_new("x".repeat(4_097)).is_err());

    assert!(FiniteF64::try_new(1.25).is_ok());
    assert!(FiniteF64::try_new(f64::NAN).is_err());
    assert!(FiniteF64::try_new(f64::INFINITY).is_err());
    assert!(FiniteF64::try_new(f64::NEG_INFINITY).is_err());

    assert!(Revision::try_new(1).is_ok());
    assert!(Revision::try_new(0).is_err());

    assert!(Uuid::try_from_bytes([0; 16]).is_ok());
    assert!(UuidV7::try_from_bytes([0; 16]).is_err());
    assert!(UuidV7::try_from_bytes(uuid_v7_bytes(1)).is_ok());

    assert!(NonEmptyVec::<u8>::try_new(Vec::new()).is_err());
    assert!(NonEmptyVec::try_new(vec![1]).is_ok());
    assert!(NonEmptySet::<u8>::try_new(BTreeSet::new()).is_err());
    assert!(NonEmptySet::try_new(BTreeSet::from([1])).is_ok());

    let canonical_path = std::env::temp_dir().join("surface");
    let parent_path = std::env::temp_dir().join("..").join("surface");
    let current_path = std::env::temp_dir().join(".").join("surface");
    let duplicate_separator_path = PathBuf::from(format!(
        "{}{}{}surface",
        std::env::temp_dir().display(),
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR,
    ));
    assert!(CanonicalPath::try_new(canonical_path).is_ok());
    assert!(CanonicalPath::try_new(PathBuf::from("relative")).is_err());
    assert!(CanonicalPath::try_new(parent_path).is_err());
    assert!(CanonicalPath::try_new(current_path).is_err());
    assert!(CanonicalPath::try_new(duplicate_separator_path.clone()).is_err());
    assert!(
        serde_json::from_value::<CanonicalPath>(serde_json::json!(duplicate_separator_path))
            .is_err()
    );
    assert!(CanonicalUri::try_new("https://example.com/path").is_ok());
    assert!(CanonicalUri::try_new("https://example.com:8443/path").is_ok());
    assert!(CanonicalUri::try_new("not a uri").is_err());
    assert!(CanonicalUri::try_new("HTTPS://example.com/path").is_err());
    assert!(CanonicalUri::try_new("https://EXAMPLE.COM/path").is_err());
    assert!(CanonicalUri::try_new("https://example.com:443/path").is_err());
    assert!(CanonicalUri::try_new("https://example.com:0443/path").is_err());
    assert!(
        serde_json::from_value::<CanonicalUri>(serde_json::json!("https://EXAMPLE.COM/path"))
            .is_err()
    );
    assert!(CanonicalMime::try_new("application/json").is_ok());
    assert!(CanonicalMime::try_new("Application/JSON").is_err());
    assert!(CanonicalMime::try_new("application/json; charset=utf-8").is_err());
    assert!(CanonicalDomainName::try_new("example.com").is_ok());
    assert!(CanonicalDomainName::try_new("*.example.com").is_err());
    assert!(CanonicalDomainName::try_new("https://example.com").is_err());
    assert!(Rfc3339Timestamp::try_new("2026-07-22T00:00:00Z").is_ok());
    assert!(Rfc3339Timestamp::try_new("2026-07-22T08:00:00+08:00").is_err());
}

#[test]
fn canonical_path_unicode_wire_shape_round_trips() {
    let path = std::env::temp_dir().join("orca-\u{8868}\u{9762}");

    let canonical = CanonicalPath::try_new(path).unwrap();
    let encoded = serde_json::to_value(&canonical).unwrap();
    let decoded: CanonicalPath = serde_json::from_value(encoded).unwrap();

    assert_eq!(decoded, canonical);
}

#[cfg(unix)]
#[test]
fn canonical_path_rejects_invalid_utf8_unix_path() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let mut bytes = b"/tmp/orca-surface-".to_vec();
    bytes.push(0xff);
    let path = PathBuf::from(OsString::from_vec(bytes));

    assert!(path.is_absolute());
    assert!(CanonicalPath::try_new(path).is_err());
}

#[cfg(windows)]
#[test]
fn canonical_path_rejects_unpaired_utf16_windows_path() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let mut wide: Vec<u16> = "C:\\tmp\\orca-surface-".encode_utf16().collect();
    wide.push(0xd800);
    let path = PathBuf::from(OsString::from_wide(&wide));

    assert!(path.is_absolute());
    assert!(CanonicalPath::try_new(path).is_err());
}

#[test]
fn canonical_uri_validates_the_complete_authority_userinfo() {
    for valid in [
        "https://user@example.com/path",
        "https://user:pa%40ss@example.com:8443/path",
    ] {
        assert!(
            CanonicalUri::try_new(valid).is_ok(),
            "constructor rejected canonical userinfo: {valid}"
        );
        assert!(
            serde_json::from_value::<CanonicalUri>(serde_json::json!(valid)).is_ok(),
            "serde rejected canonical userinfo: {valid}"
        );
    }

    for invalid in [
        "https://@example.com/path",
        "https://user@@example.com/path",
        "https://user[admin]@example.com/path",
        "https://user%@example.com/path",
        "https://user%4@example.com/path",
        "https://user%GG@example.com/path",
        "https://user%4a@example.com/path",
        "https://%75ser@example.com/path",
    ] {
        assert!(
            CanonicalUri::try_new(invalid).is_err(),
            "constructor accepted noncanonical userinfo: {invalid}"
        );
        assert!(
            serde_json::from_value::<CanonicalUri>(serde_json::json!(invalid)).is_err(),
            "serde accepted noncanonical userinfo: {invalid}"
        );
    }
}

#[test]
fn reservation_lease_has_one_canonical_v1_duration() {
    let canonical = serde_json::json!({
        "lease_id": SurfaceAdmissionLeaseId::try_from_bytes(uuid_v7_bytes(21)).unwrap(),
        "operation_id": SurfaceOperationId::try_from_bytes(uuid_v7_bytes(22)).unwrap(),
        "reservation_sequence": SequenceNumber::new(7),
        "issuing_host_incarnation": HostIncarnation::try_from_bytes(uuid_v7_bytes(23)).unwrap(),
        "issued_at": MonotonicInstant {
            clock_id: HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(24)).unwrap(),
            tick: MonotonicTick::new(11),
        },
        "duration": SURFACE_RESERVATION_LEASE_MS,
    });
    let lease: ReservationLease = serde_json::from_value(canonical.clone()).unwrap();
    assert_eq!(SURFACE_RESERVATION_LEASE_MS, 30_000);
    assert_eq!(lease.duration().get(), SURFACE_RESERVATION_LEASE_MS);

    let serialized = serde_json::to_value(&lease).unwrap();
    assert_eq!(
        serialized.get("duration"),
        Some(&serde_json::json!(SURFACE_RESERVATION_LEASE_MS))
    );
    assert_eq!(serialized, canonical);

    for invalid_duration in [0, 29_999, 30_001] {
        let mut invalid = canonical.clone();
        invalid["duration"] = serde_json::json!(invalid_duration);
        assert!(
            serde_json::from_value::<ReservationLease>(invalid).is_err(),
            "accepted noncanonical lease duration {invalid_duration}"
        );
    }
}

#[test]
fn operation_generation_and_terminal_algebras_are_closed() {
    let generation_id = SurfaceGenerationId::new(0);
    let diagnostic = SafeDiagnosticText::try_new("bounded failure").unwrap();
    let budget = OperationBudget::TurnRequests {
        scope: TurnRequestBudgetScope::AgentLoop,
        limit: 128,
        observed: 128,
    };
    let usage = UsageTotals {
        input_tokens: 1,
        output_tokens: 2,
        cache_tokens: 3,
        estimated_cost_usd_micros: 4,
    };

    let phases = [
        OperationPhase::Requested,
        OperationPhase::Admitted,
        OperationPhase::Suspended {
            cause: SuspensionCause::Interrupted { generation_id },
        },
        OperationPhase::Finalizing {
            finalize_intent_id: SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(31)).unwrap(),
        },
        OperationPhase::FinalizingDegraded {
            finalize_intent_id: SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(32)).unwrap(),
        },
        OperationPhase::Terminal,
    ];
    for phase in phases {
        match phase {
            OperationPhase::Requested
            | OperationPhase::Admitted
            | OperationPhase::Suspended { .. }
            | OperationPhase::Finalizing { .. }
            | OperationPhase::FinalizingDegraded { .. }
            | OperationPhase::Terminal => {}
        }
    }

    for phase in [
        GenerationPhase::Reserved,
        GenerationPhase::Started,
        GenerationPhase::Transferred,
        GenerationPhase::Stopped,
    ] {
        match phase {
            GenerationPhase::Reserved
            | GenerationPhase::Started
            | GenerationPhase::Transferred
            | GenerationPhase::Stopped => {}
        }
    }

    let causes = [
        TerminalizationCause::UserCancel,
        TerminalizationCause::GoalPause,
        TerminalizationCause::HostShutdown,
        TerminalizationCause::ThreadClose,
    ];
    for cause in causes {
        match cause {
            TerminalizationCause::UserCancel
            | TerminalizationCause::GoalPause
            | TerminalizationCause::HostShutdown
            | TerminalizationCause::ThreadClose => {}
        }
    }

    let terminals = [
        OperationTerminal::NotAdmitted {
            reason: NotAdmittedReason::ReservationExpired,
        },
        OperationTerminal::Succeeded {
            usage: usage.clone(),
        },
        OperationTerminal::Cancelled {
            reason: CancelReason::User,
        },
        OperationTerminal::BudgetExhausted {
            budget: budget.clone(),
        },
        OperationTerminal::Failed {
            class: FailureClass::Provider,
            message: diagnostic.clone(),
        },
        OperationTerminal::Panicked {
            message: diagnostic.clone(),
        },
        OperationTerminal::JoinFailed {
            message: diagnostic.clone(),
        },
        OperationTerminal::AbortedByRuntimeRestart {
            last_generation: generation_id,
        },
        OperationTerminal::Shutdown {
            reason: SurfaceShutdownReason::HostShutdown,
        },
    ];
    assert_eq!(terminals.len(), 9);
    for terminal in terminals {
        match terminal {
            OperationTerminal::NotAdmitted { .. }
            | OperationTerminal::Succeeded { .. }
            | OperationTerminal::Cancelled { .. }
            | OperationTerminal::BudgetExhausted { .. }
            | OperationTerminal::Failed { .. }
            | OperationTerminal::Panicked { .. }
            | OperationTerminal::JoinFailed { .. }
            | OperationTerminal::AbortedByRuntimeRestart { .. }
            | OperationTerminal::Shutdown { .. } => {}
        }
    }

    let json = serde_json::to_string(&OperationTerminal::Succeeded { usage }).unwrap();
    let round_trip: OperationTerminal = serde_json::from_str(&json).unwrap();
    assert!(matches!(round_trip, OperationTerminal::Succeeded { .. }));
}

#[test]
fn every_generation_stop_and_finalizer_source_is_constructible() {
    let diagnostic = SafeDiagnosticText::try_new("failure").unwrap();
    let budget = OperationBudget::ModelTokens {
        limit: Some(1),
        observed: Some(2),
    };
    let stop_reasons = [
        GenerationStopReason::Completed {
            status: GenerationCompletionStatus::Success,
        },
        GenerationStopReason::Completed {
            status: GenerationCompletionStatus::VerificationFailed {
                message: diagnostic.clone(),
            },
        },
        GenerationStopReason::Completed {
            status: GenerationCompletionStatus::BudgetExhausted { budget },
        },
        GenerationStopReason::Cancelled {
            cause: TerminalizationCause::UserCancel,
        },
        GenerationStopReason::InterruptedResumable,
        GenerationStopReason::ProviderSuspended,
        GenerationStopReason::RuntimeRestart,
        GenerationStopReason::ProjectionFailure {
            message: diagnostic.clone(),
        },
        GenerationStopReason::ExecutionFailed {
            class: GenerationExecutionFailureClass::ExternalEffectAmbiguous,
            message: diagnostic.clone(),
        },
        GenerationStopReason::Panicked {
            message: diagnostic.clone(),
        },
        GenerationStopReason::NotStarted {
            reason: NotStartedReason::StartCommitFailure {
                message: diagnostic.clone(),
            },
        },
    ];
    for reason in stop_reasons {
        match reason {
            GenerationStopReason::Completed { .. }
            | GenerationStopReason::Cancelled { .. }
            | GenerationStopReason::InterruptedResumable
            | GenerationStopReason::ProviderSuspended
            | GenerationStopReason::RuntimeRestart
            | GenerationStopReason::ProjectionFailure { .. }
            | GenerationStopReason::ExecutionFailed { .. }
            | GenerationStopReason::Panicked { .. }
            | GenerationStopReason::NotStarted { .. } => {}
        }
    }

    let operation_id = operation_fence(41).operation_id;
    let finalize_intent_id = SurfaceFinalizeIntentId::try_from_bytes(uuid_v7_bytes(42)).unwrap();
    let join = OperationJoinSettlementSource {
        operation_id,
        finalize_intent_id,
        settlement_id: SurfaceSettlementId::try_from_bytes(uuid_v7_bytes(43)).unwrap(),
        settlement_receipt_digest: Sha256Digest::new([1; 32]),
        message: diagnostic,
    };
    let sources = [
        OperationFinalizerSource::GenerationStop {
            reason: GenerationStopReason::RuntimeRestart,
        },
        OperationFinalizerSource::Reservation {
            source: ReservationFinalizerSource {
                reason: ReservationFinalizerReason::RuntimeRestart,
            },
        },
        OperationFinalizerSource::OperationJoinSettlement { source: join },
    ];
    for source in sources {
        match source {
            OperationFinalizerSource::GenerationStop { .. }
            | OperationFinalizerSource::Reservation { .. }
            | OperationFinalizerSource::OperationJoinSettlement { .. } => {}
        }
    }

    for cause in [
        SuspendedFinalizationCause::Terminalization(TerminalizationCause::UserCancel),
        SuspendedFinalizationCause::ResumeStartCommitFailure {
            message: SafeDiagnosticText::try_new("commit").unwrap(),
        },
        SuspendedFinalizationCause::RecoveryAbortNonReplayable {
            last_generation: SurfaceGenerationId::new(1),
        },
    ] {
        match cause {
            SuspendedFinalizationCause::Terminalization(_)
            | SuspendedFinalizationCause::ResumeStartCommitFailure { .. }
            | SuspendedFinalizationCause::RecoveryAbortNonReplayable { .. } => {}
        }
    }
}

#[test]
fn operation_input_and_replayability_values_are_closed() {
    let blocks = NonEmptyVec::try_new(vec![SurfaceInputRequestBlock::Text {
        text: DisplayText::new("prompt"),
    }])
    .unwrap();
    let request = SurfaceInputRequest { blocks };
    let kinds = [
        OperationKind::UserTurn,
        OperationKind::GoalRun {
            goal_id: SurfaceGoalId::try_new("goal").unwrap(),
            goal_run_id: SurfaceGoalRunId::try_new("run").unwrap(),
            initial_objective_revision: GoalObjectiveRevision::new(0),
        },
        OperationKind::ManualCompaction {
            reason: ManualCompactionReason::Manual,
        },
        OperationKind::Backtrack {
            target: LastUserTurn::MostRecent,
        },
        OperationKind::StandaloneWorkflow {
            workflow: SurfaceCatalogEntryId::try_new("workflow").unwrap(),
        },
        OperationKind::WorkflowResultFollowup {
            result_id: SurfaceWorkflowResultId::try_new("result").unwrap(),
        },
    ];
    for kind in kinds {
        match kind {
            OperationKind::UserTurn
            | OperationKind::GoalRun { .. }
            | OperationKind::ManualCompaction { .. }
            | OperationKind::Backtrack { .. }
            | OperationKind::StandaloneWorkflow { .. }
            | OperationKind::WorkflowResultFollowup { .. } => {}
        }
    }

    let _intent = OperationRequestIntent {
        correlation: OperationIngressCorrelation::TuiUser,
        kind: OperationKind::UserTurn,
        input: Some(request),
        replayability: ReplayabilityRequest::CaptureReplayableCapsule,
        settings_preparation: OperationSettingsPreparation::UseCurrent {
            expected_settings_revision: SettingsRevision::try_new(1).unwrap(),
            expected_policy_epoch: PolicyEpoch::try_new(1).unwrap(),
        },
    };
    for replayability in [
        ReplayabilityClass::Replayable,
        ReplayabilityClass::NonReplayable(LiveCapsuleStatus::Current),
        ReplayabilityClass::NonReplayable(LiveCapsuleStatus::NotCurrent {
            descriptor: StaleLiveCapsuleDescriptor::Unavailable,
        }),
    ] {
        match replayability {
            ReplayabilityClass::Replayable | ReplayabilityClass::NonReplayable(_) => {}
        }
    }
}

#[test]
fn interaction_kinds_routes_answers_and_lifecycles_are_closed() {
    let kinds = [
        SurfaceInteractionKind::ToolApproval,
        SurfaceInteractionKind::PermissionRequest,
        SurfaceInteractionKind::UserInput,
        SurfaceInteractionKind::McpElicitation,
        SurfaceInteractionKind::BackgroundApproval,
    ];
    assert_eq!(kinds.len(), 5);
    for kind in kinds {
        match kind {
            SurfaceInteractionKind::ToolApproval
            | SurfaceInteractionKind::PermissionRequest
            | SurfaceInteractionKind::UserInput
            | SurfaceInteractionKind::McpElicitation
            | SurfaceInteractionKind::BackgroundApproval => {}
        }
    }

    let revision = ResponseRouteEpoch::try_new(1).unwrap();
    let attachment = SurfaceAttachmentId::try_from_bytes(uuid_v7_bytes(51)).unwrap();
    let routes = [
        SurfaceInteractionRoute::Unassigned { epoch: revision },
        SurfaceInteractionRoute::Exclusive {
            epoch: revision,
            attachment_id: attachment.clone(),
        },
        SurfaceInteractionRoute::SharedFirstCommitWins {
            epoch: revision,
            attachments: NonEmptySet::try_new(BTreeSet::from([attachment])).unwrap(),
        },
    ];
    for route in routes {
        match route {
            SurfaceInteractionRoute::Unassigned { .. }
            | SurfaceInteractionRoute::Exclusive { .. }
            | SurfaceInteractionRoute::SharedFirstCommitWins { .. } => {}
        }
    }

    let answers = [
        SurfaceClientInteractionAnswer::ToolApproval {
            decision: SurfaceAllowDeny::Allow,
        },
        SurfaceClientInteractionAnswer::PermissionRequest {
            decision: SurfacePermissionClientDecision::Deny {
                scope: PermissionGrantScope::Turn,
                permissions: SurfacePermissionProfile::empty(),
                strict_auto_review: false,
            },
        },
        SurfaceClientInteractionAnswer::UserInput {
            decision: SurfaceUserInputDecision::Answer(DisplayText::new("")),
        },
        SurfaceClientInteractionAnswer::McpElicitation {
            decision: SurfaceMcpElicitationDecision::Accept {
                content: SurfaceDataValue::Object(Vec::new()),
            },
        },
        SurfaceClientInteractionAnswer::BackgroundApproval {
            decision: SurfaceAllowDeny::Deny,
        },
    ];
    for answer in answers {
        match answer {
            SurfaceClientInteractionAnswer::ToolApproval { .. }
            | SurfaceClientInteractionAnswer::PermissionRequest { .. }
            | SurfaceClientInteractionAnswer::UserInput { .. }
            | SurfaceClientInteractionAnswer::McpElicitation { .. }
            | SurfaceClientInteractionAnswer::BackgroundApproval { .. } => {}
        }
    }

    let receipt = SurfaceInteractionResolutionReceipt {
        response_id: SurfaceResponseId::try_from_bytes(uuid_v7_bytes(52)).unwrap(),
        receipt_id: SurfaceResponseReceiptId::try_from_bytes(uuid_v7_bytes(53)).unwrap(),
        kind: SurfaceInteractionKind::UserInput,
        safe_projection: SurfaceInteractionSafeProjection::UserInput { answered: true },
    };
    let deadline = InteractionExpiryDeadline {
        issuing_host_incarnation: HostIncarnation::try_from_bytes(uuid_v7_bytes(54)).unwrap(),
        expires_at: MonotonicInstant {
            clock_id: HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(55)).unwrap(),
            tick: MonotonicTick::new(10),
        },
        observed_expires_at: Some(UnixMillis::new(10)),
    };
    let lifecycles = [
        SurfaceInteractionLifecycle::Requested,
        SurfaceInteractionLifecycle::Resolved { receipt },
        SurfaceInteractionLifecycle::Cancelled {
            reason: InteractionCancelReason::HostShutdown,
        },
        SurfaceInteractionLifecycle::Expired { deadline },
    ];
    for lifecycle in lifecycles {
        match lifecycle {
            SurfaceInteractionLifecycle::Requested
            | SurfaceInteractionLifecycle::Resolved { .. }
            | SurfaceInteractionLifecycle::Cancelled { .. }
            | SurfaceInteractionLifecycle::Expired { .. }
            | SurfaceInteractionLifecycle::Transferred { .. } => {}
        }
    }
}

#[test]
fn schemas_and_closed_data_reject_noncanonical_numbers_and_bounds() {
    assert!(NegativeI64::try_new(-1).is_ok());
    assert!(NegativeI64::try_new(0).is_err());
    assert!(NegativeI64::try_new(1).is_err());
    assert!(SurfaceSchemaInteger::try_negative(-1).is_ok());
    assert!(SurfaceSchemaInteger::try_negative(0).is_err());
    let schema = SurfaceSchema::Object {
        title: None,
        description: None,
        properties: vec![SurfaceSchemaProperty {
            name: DisplayText::new("answer"),
            required: true,
            schema: Box::new(SurfaceSchema::String {
                title: None,
                description: None,
                enum_values: Vec::new(),
                min_length: Some(1),
                max_length: Some(10),
            }),
        }],
        additional_properties: (),
    };
    let json = serde_json::to_string(&schema).unwrap();
    assert_eq!(
        serde_json::from_str::<SurfaceSchema>(&json).unwrap(),
        schema
    );

    let data = SurfaceDataValue::Array(vec![
        SurfaceDataValue::Null,
        SurfaceDataValue::Boolean(true),
        SurfaceDataValue::Integer(NegativeI64::try_new(-1).unwrap()),
        SurfaceDataValue::Unsigned(0),
        SurfaceDataValue::Number(FiniteF64::try_new(1.5).unwrap()),
        SurfaceDataValue::String(DisplayText::new("value")),
        SurfaceDataValue::Object(vec![SurfaceDataProperty {
            name: DisplayText::new("key"),
            value: Box::new(SurfaceDataValue::Null),
        }]),
    ]);
    let json = serde_json::to_string(&data).unwrap();
    assert_eq!(
        serde_json::from_str::<SurfaceDataValue>(&json).unwrap(),
        data
    );
}

#[test]
fn all_public_interaction_patch_variants_are_constructible() {
    let interaction_id = SurfaceInteractionId::try_from_bytes(uuid_v7_bytes(61)).unwrap();
    let revision = InteractionRevision::try_new(1).unwrap();
    let next_revision = InteractionRevision::try_new(2).unwrap();
    let route = SurfaceInteractionRoute::Unassigned {
        epoch: ResponseRouteEpoch::try_new(1).unwrap(),
    };
    let deadline = InteractionExpiryDeadline {
        issuing_host_incarnation: HostIncarnation::try_from_bytes(uuid_v7_bytes(62)).unwrap(),
        expires_at: MonotonicInstant {
            clock_id: HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(63)).unwrap(),
            tick: MonotonicTick::new(1),
        },
        observed_expires_at: None,
    };
    let receipt = SurfaceInteractionResolutionReceipt {
        response_id: SurfaceResponseId::try_from_bytes(uuid_v7_bytes(64)).unwrap(),
        receipt_id: SurfaceResponseReceiptId::try_from_bytes(uuid_v7_bytes(65)).unwrap(),
        kind: SurfaceInteractionKind::ToolApproval,
        safe_projection: SurfaceInteractionSafeProjection::ToolApproval { allowed: true },
    };
    let patches = [
        InteractionPatch::RouteChanged {
            interaction_id: interaction_id.clone(),
            expected_revision: revision,
            next_revision,
            route: route.clone(),
        },
        InteractionPatch::Resolved {
            interaction_id: interaction_id.clone(),
            expected_revision: revision,
            next_revision,
            receipt,
            continuation: None,
        },
        InteractionPatch::Cancelled {
            interaction_id: interaction_id.clone(),
            expected_revision: revision,
            next_revision,
            reason: InteractionCancelReason::ThreadClose,
        },
        InteractionPatch::Expired {
            interaction_id,
            expected_revision: revision,
            next_revision,
            deadline,
        },
    ];
    for patch in patches {
        match patch {
            InteractionPatch::Requested { .. }
            | InteractionPatch::RouteChanged { .. }
            | InteractionPatch::Resolved { .. }
            | InteractionPatch::ContinuationDispatchStarted { .. }
            | InteractionPatch::ContinuationDispatchConsumed { .. }
            | InteractionPatch::Cancelled { .. }
            | InteractionPatch::Expired { .. }
            | InteractionPatch::Transferred { .. } => {}
        }
    }
}

fn operation_patch_name(value: &OperationPatch) -> &'static str {
    match value {
        OperationPatch::Requested { .. } => "Requested",
        OperationPatch::ReservationQueueChanged { .. } => "ReservationQueueChanged",
        OperationPatch::Admitted { .. } => "Admitted",
        OperationPatch::InputBindingsResolved { .. } => "InputBindingsResolved",
        OperationPatch::InputBindingsFailed { .. } => "InputBindingsFailed",
        OperationPatch::ControlIntentCommitted { .. } => "ControlIntentCommitted",
        OperationPatch::GenerationReserved { .. } => "GenerationReserved",
        OperationPatch::GenerationStarted { .. } => "GenerationStarted",
        OperationPatch::AgentLoopTurnStarted { .. } => "AgentLoopTurnStarted",
        OperationPatch::ModelRouteSelected { .. } => "ModelRouteSelected",
        OperationPatch::VerificationStarted { .. } => "VerificationStarted",
        OperationPatch::VerificationCompleted { .. } => "VerificationCompleted",
        OperationPatch::GenerationStopped { .. } => "GenerationStopped",
        OperationPatch::GenerationTransferred { .. } => "GenerationTransferred",
        OperationPatch::Suspended { .. } => "Suspended",
        OperationPatch::SuspensionRebasedAfterUnstartedResume { .. } => {
            "SuspensionRebasedAfterUnstartedResume"
        }
        OperationPatch::RecoveryRequired { .. } => "RecoveryRequired",
        OperationPatch::FinalizationStarted { .. } => "FinalizationStarted",
        OperationPatch::FinalizationSettlementRecorded { .. } => "FinalizationSettlementRecorded",
        OperationPatch::FinalizationDegraded { .. } => "FinalizationDegraded",
        OperationPatch::Terminal { .. } => "Terminal",
    }
}

macro_rules! patch_name_matcher {
    ($function:ident, $type:ident, [$($variant:ident),+ $(,)?]) => {
        fn $function(value: &$type) -> &'static str {
            match value {
                $($type::$variant { .. } => stringify!($variant),)+
            }
        }
    };
}

patch_name_matcher!(
    item_patch_name,
    ItemPatch,
    [Added, InputResolved, InputResolutionFailed, Removed]
);
patch_name_matcher!(
    assistant_patch_name,
    AssistantPatch,
    [StreamOpened, Delta, ResponseCompleted, StreamDiscarded]
);
patch_name_matcher!(
    tool_patch_name,
    ToolPatch,
    [
        Requested,
        ArgumentsProgress,
        OutputDelta,
        InvocationStartedV1,
        Completed,
        CapabilityCallChanged,
        RemoteTerminalLeaseChanged
    ]
);
patch_name_matcher!(
    interaction_patch_name,
    InteractionPatch,
    [
        Requested,
        RouteChanged,
        Resolved,
        ContinuationDispatchStarted,
        ContinuationDispatchConsumed,
        Cancelled,
        Expired,
        Transferred
    ]
);
patch_name_matcher!(
    task_patch_name,
    TaskPatch,
    [
        Upserted,
        StatusChanged,
        InteractionChanged,
        OwnershipChanged,
        Reconciled
    ]
);
patch_name_matcher!(
    workflow_patch_name,
    WorkflowPatch,
    [
        Started,
        Resumed,
        PhaseStarted,
        PhaseCompleted,
        AgentStarted,
        AgentCached,
        AgentCompleted,
        AgentFailed,
        AgentCancelled,
        Paused,
        Stopping,
        Stopped,
        AsyncLaunched,
        Completed,
        Failed,
        Cancelled,
        ResultReady,
        ResultAcknowledged
    ]
);
patch_name_matcher!(
    subagent_patch_name,
    SubagentPatch,
    [Started, Progress, Completed]
);
patch_name_matcher!(
    goal_patch_name,
    GoalPatch,
    [
        Created,
        Edited,
        Removed,
        RunStarted,
        OuterTurnStarted,
        IntentRequested,
        IntentAcknowledged,
        OuterTurnFinished,
        VerificationCompleted,
        Transitioned,
        ContinuationDecided,
        Paused,
        Recovered,
        Completed
    ]
);
patch_name_matcher!(
    settings_patch_name,
    SettingsPatch,
    [Committed, PendingChanged]
);
patch_name_matcher!(
    mcp_patch_name,
    McpCatalogPatch,
    [Reconciled, ServerStatusChanged]
);
patch_name_matcher!(
    pinned_patch_name,
    PinnedContextPatch,
    [Added, Removed, Reconciled]
);
patch_name_matcher!(
    session_patch_name,
    SessionPatch,
    [
        Materialized,
        OwnerEpochChanged,
        MetadataChanged,
        HealthIssueAdded,
        HealthIssueCleared,
        RuntimeFault,
        Closing,
        Closed
    ]
);

fn subagent_patch_refinements_are_exact(value: &SubagentPatch) {
    if let SubagentPatch::Started {
        expected_revision,
        subagent,
    } = value
    {
        let _: &ExpectedAbsentSubagentRevision = expected_revision;
        let _: &RunningSurfaceSubagent = subagent;
    }
}

fn goal_stop_reason_refinements_are_exact(value: &GoalContinuationStopReason) {
    if let GoalContinuationStopReason::BudgetLimited { budget } = value {
        let _: &GoalTokenBudget = budget;
    }
}

fn goal_patch_refinements_are_exact(value: &GoalPatch) {
    if let GoalPatch::Recovered {
        discarded_continuation,
        ..
    } = value
    {
        let _: &DiscardedContinuation = discarded_continuation;
    }
}

#[test]
fn subagent_and_goal_projection_refinements_are_exact() {
    let _: fn(&SubagentPatch) = subagent_patch_refinements_are_exact;
    let _: fn(&GoalContinuationStopReason) = goal_stop_reason_refinements_are_exact;
    let _: fn(&GoalPatch) = goal_patch_refinements_are_exact;

    let expected_absent = ExpectedAbsentSubagentRevision;
    assert_eq!(
        serde_json::to_value(expected_absent).unwrap(),
        serde_json::Value::Null
    );
    assert!(
        serde_json::from_value::<ExpectedAbsentSubagentRevision>(serde_json::Value::Null).is_ok()
    );
    assert!(
        serde_json::from_value::<ExpectedAbsentSubagentRevision>(serde_json::json!(1)).is_err()
    );

    let running = SurfaceSubagent {
        subagent_id: SurfaceSubagentId::try_new("subagent-1").unwrap(),
        task_id: SurfaceTaskId::try_new("task-1").unwrap(),
        revision: SubagentRevision::try_new(1).unwrap(),
        description: DisplayText::new("focused projection"),
        status: SurfaceSubagentStatus::Running,
        activity: None,
        turn: None,
        usage: None,
        output: None,
        error: None,
        owner: SurfaceSubagentOwner::Generation {
            fence: operation_fence(41),
        },
        source: SurfaceSubagentSource::new(
            SurfaceTaskAttemptId::try_new("attempt-41").unwrap(),
            1,
            SurfaceCommitId::try_from_bytes(uuid_v7_bytes(42)).unwrap(),
            Sha256Digest::new([41; 32]),
        ),
    };
    let running_refinement = RunningSurfaceSubagent::try_new(running.clone()).unwrap();
    assert_eq!(running_refinement.as_subagent(), &running);
    assert_eq!(
        serde_json::to_value(&running_refinement).unwrap(),
        serde_json::to_value(&running).unwrap()
    );

    let mut completed = running;
    completed.status = SurfaceSubagentStatus::Completed;
    assert!(RunningSurfaceSubagent::try_new(completed.clone()).is_err());
    assert!(
        serde_json::from_value::<RunningSurfaceSubagent>(serde_json::to_value(completed).unwrap())
            .is_err()
    );

    let discarded = DiscardedContinuation::new();
    assert!(discarded.get());
    assert_eq!(
        serde_json::to_value(discarded).unwrap(),
        serde_json::json!(true)
    );
    assert!(serde_json::from_value::<DiscardedContinuation>(serde_json::json!(true)).is_ok());
    assert!(serde_json::from_value::<DiscardedContinuation>(serde_json::json!(false)).is_err());

    let goal_budget = OperationBudget::GoalTokenBudget {
        goal_id: SurfaceGoalId::try_new("goal-1").unwrap(),
        limit: 10_000,
        observed: 10_000,
    };
    let goal_budget_refinement = GoalTokenBudget::try_new(goal_budget.clone()).unwrap();
    assert_eq!(goal_budget_refinement.as_budget(), &goal_budget);
    assert_eq!(
        serde_json::to_value(&goal_budget_refinement).unwrap(),
        serde_json::to_value(&goal_budget).unwrap()
    );
    let non_goal_budget = OperationBudget::ModelTokens {
        limit: Some(10_000),
        observed: Some(10_000),
    };
    assert!(GoalTokenBudget::try_new(non_goal_budget.clone()).is_err());
    assert!(
        serde_json::from_value::<GoalTokenBudget>(serde_json::to_value(non_goal_budget).unwrap())
            .is_err()
    );
}

#[test]
fn every_manifest_patch_inventory_has_an_exhaustive_rust_match() {
    let manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../../../docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json"
    ))
    .unwrap();
    let inventory = &manifest["closed_inventory"];
    let expected: &[(&str, &[&str])] = &[
        (
            "operation_patch_variants",
            &[
                "Requested",
                "ReservationQueueChanged",
                "Admitted",
                "InputBindingsResolved",
                "InputBindingsFailed",
                "ControlIntentCommitted",
                "GenerationReserved",
                "GenerationStarted",
                "AgentLoopTurnStarted",
                "ModelRouteSelected",
                "VerificationStarted",
                "VerificationCompleted",
                "GenerationStopped",
                "GenerationTransferred",
                "Suspended",
                "SuspensionRebasedAfterUnstartedResume",
                "RecoveryRequired",
                "FinalizationStarted",
                "FinalizationSettlementRecorded",
                "FinalizationDegraded",
                "Terminal",
            ],
        ),
        (
            "item_patch_variants",
            &["Added", "InputResolved", "InputResolutionFailed", "Removed"],
        ),
        (
            "assistant_patch_variants",
            &[
                "StreamOpened",
                "Delta",
                "ResponseCompleted",
                "StreamDiscarded",
            ],
        ),
        (
            "tool_patch_variants",
            &[
                "Requested",
                "ArgumentsProgress",
                "OutputDelta",
                "Completed",
                "CapabilityCallChanged",
                "RemoteTerminalLeaseChanged",
            ],
        ),
        (
            "interaction_patch_variants",
            &[
                "Requested",
                "RouteChanged",
                "Resolved",
                "Cancelled",
                "Expired",
                "Transferred",
            ],
        ),
        (
            "task_patch_variants",
            &[
                "Upserted",
                "StatusChanged",
                "InteractionChanged",
                "OwnershipChanged",
                "Reconciled",
            ],
        ),
        (
            "workflow_patch_variants",
            &[
                "Started",
                "Resumed",
                "PhaseStarted",
                "PhaseCompleted",
                "AgentStarted",
                "AgentCached",
                "AgentCompleted",
                "AgentFailed",
                "AgentCancelled",
                "Paused",
                "Stopping",
                "Stopped",
                "AsyncLaunched",
                "Completed",
                "Failed",
                "Cancelled",
                "ResultReady",
                "ResultAcknowledged",
            ],
        ),
        (
            "subagent_patch_variants",
            &[
                "Started",
                "Progress",
                "Completed",
                "ContinuationCheckpointed",
                "ContinuationSuspended",
                "ContinuationResumed",
                "ContinuationOrphanReconciled",
                "ContinuationIndeterminate",
            ],
        ),
        (
            "goal_patch_variants",
            &[
                "Created",
                "Edited",
                "Removed",
                "RunStarted",
                "OuterTurnStarted",
                "IntentRequested",
                "IntentAcknowledged",
                "OuterTurnFinished",
                "VerificationCompleted",
                "Transitioned",
                "ContinuationDecided",
                "Paused",
                "Recovered",
                "Completed",
            ],
        ),
        ("settings_patch_variants", &["Committed", "PendingChanged"]),
        (
            "mcp_catalog_patch_variants",
            &["Reconciled", "ServerStatusChanged"],
        ),
        (
            "pinned_context_patch_variants",
            &["Added", "Removed", "Reconciled"],
        ),
        (
            "session_patch_variants",
            &[
                "Materialized",
                "OwnerEpochChanged",
                "MetadataChanged",
                "HealthIssueAdded",
                "HealthIssueCleared",
                "RuntimeFault",
                "Closing",
                "Closed",
            ],
        ),
    ];
    for (key, variants) in expected {
        let actual = inventory[*key]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, *variants, "manifest inventory drift for {key}");
    }

    let _matchers = (
        operation_patch_name as fn(&OperationPatch) -> _,
        item_patch_name as fn(&ItemPatch) -> _,
        assistant_patch_name as fn(&AssistantPatch) -> _,
        tool_patch_name as fn(&ToolPatch) -> _,
        interaction_patch_name as fn(&InteractionPatch) -> _,
        task_patch_name as fn(&TaskPatch) -> _,
        workflow_patch_name as fn(&WorkflowPatch) -> _,
        subagent_patch_name as fn(&SubagentPatch) -> _,
        goal_patch_name as fn(&GoalPatch) -> _,
        settings_patch_name as fn(&SettingsPatch) -> _,
        mcp_patch_name as fn(&McpCatalogPatch) -> _,
        pinned_patch_name as fn(&PinnedContextPatch) -> _,
        session_patch_name as fn(&SessionPatch) -> _,
    );
}

fn surface_command_name(command: &SurfaceCommand) -> &'static str {
    match command {
        SurfaceCommand::ReserveOperation {
            request_id,
            caller,
            intent,
        } => {
            let _ = (request_id, caller, intent);
            "ReserveOperation"
        }
        SurfaceCommand::AdmitReserved {
            request_id,
            caller,
            operation_id,
            admission_lease_id,
        } => {
            let _ = (request_id, caller, operation_id, admission_lease_id);
            "AdmitReserved"
        }
        SurfaceCommand::CancelOperation {
            request_id,
            caller,
            operation_id,
        } => {
            let _ = (request_id, caller, operation_id);
            "CancelOperation"
        }
        SurfaceCommand::CancelSessionCurrent {
            request_id,
            caller,
            legacy_rpc_id_digest,
        } => {
            let _ = (request_id, caller, legacy_rpc_id_digest);
            "CancelSessionCurrent"
        }
        SurfaceCommand::InterruptGeneration {
            request_id,
            caller,
            fence,
        } => {
            let _ = (request_id, caller, fence);
            "InterruptGeneration"
        }
        SurfaceCommand::PauseGoalOperation {
            request_id,
            caller,
            goal_fence,
        } => {
            let _ = (request_id, caller, goal_fence);
            "PauseGoalOperation"
        }
        SurfaceCommand::ResumeOperation {
            request_id,
            caller,
            operation_id,
            expected_last_generation,
            resume_source,
        } => {
            let _ = (
                request_id,
                caller,
                operation_id,
                expected_last_generation,
                resume_source,
            );
            "ResumeOperation"
        }
        SurfaceCommand::SteerOperation {
            request_id,
            caller,
            fence,
            input,
        } => {
            let _ = (request_id, caller, fence, input);
            "SteerOperation"
        }
        SurfaceCommand::TransferBackground {
            request_id,
            caller,
            target,
        } => {
            let _ = (request_id, caller, target);
            "TransferBackground"
        }
        SurfaceCommand::RespondInteraction {
            request_id,
            caller,
            selector,
            response,
        } => {
            let _ = (request_id, caller, selector, response);
            "RespondInteraction"
        }
        SurfaceCommand::ReconcileMutation { token } => {
            let _ = token;
            "ReconcileMutation"
        }
        SurfaceCommand::RetryStartCommit { token } => {
            let _ = token;
            "RetryStartCommit"
        }
        SurfaceCommand::RetryProjection { token } => {
            let _ = token;
            "RetryProjection"
        }
        SurfaceCommand::RetryFinalization { token } => {
            let _ = token;
            "RetryFinalization"
        }
        SurfaceCommand::ManualCompact {
            request_id,
            caller,
            expected_context_revision,
        } => {
            let _ = (request_id, caller, expected_context_revision);
            "ManualCompact"
        }
        SurfaceCommand::Backtrack {
            request_id,
            caller,
            expected_cursor,
            target,
        } => {
            let _ = (request_id, caller, expected_cursor, target);
            "Backtrack"
        }
        SurfaceCommand::TaskControl {
            request_id,
            caller,
            action,
        } => {
            let _ = (request_id, caller, action);
            "TaskControl"
        }
        SurfaceCommand::WorkflowControl {
            request_id,
            caller,
            action,
        } => {
            let _ = (request_id, caller, action);
            "WorkflowControl"
        }
        SurfaceCommand::GoalMutation {
            request_id,
            caller,
            action,
        } => {
            let _ = (request_id, caller, action);
            "GoalMutation"
        }
        SurfaceCommand::SettingsMutation {
            request_id,
            caller,
            host_incarnation,
            expected_thread_revision,
            patch,
        } => {
            let _ = (
                request_id,
                caller,
                host_incarnation,
                expected_thread_revision,
                patch,
            );
            "SettingsMutation"
        }
        SurfaceCommand::McpCatalogQuery {
            request_id,
            caller,
            expected_revision,
            query,
        } => {
            let _ = (request_id, caller, expected_revision, query);
            "McpCatalogQuery"
        }
        SurfaceCommand::PinnedContextMutation {
            request_id,
            caller,
            action,
        } => {
            let _ = (request_id, caller, action);
            "PinnedContextMutation"
        }
    }
}

fn surface_host_command_name(command: &SurfaceHostCommand) -> &'static str {
    match command {
        SurfaceHostCommand::ListSessions { request_id, page } => {
            let _ = (request_id, page);
            "ListSessions"
        }
        SurfaceHostCommand::SearchSessions { request_id, search } => {
            let _ = (request_id, search);
            "SearchSessions"
        }
        SurfaceHostCommand::ReadSessionMetadata {
            request_id,
            thread_id,
        } => {
            let _ = (request_id, thread_id);
            "ReadSessionMetadata"
        }
        SurfaceHostCommand::ReadSession {
            request_id,
            thread_id,
            include_messages,
            include_turns,
        } => {
            let _ = (request_id, thread_id, include_messages, include_turns);
            "ReadSession"
        }
        SurfaceHostCommand::ReadThreadPage {
            request_id,
            thread_id,
            query,
            read_token,
            cursor,
            limit,
        } => {
            let _ = (request_id, thread_id, query, read_token, cursor, limit);
            "ReadThreadPage"
        }
        SurfaceHostCommand::CreateThread { request_id, spec } => {
            let _ = (request_id, spec);
            "CreateThread"
        }
        SurfaceHostCommand::OpenThread {
            request_id,
            thread_id,
            mode,
            expected_settings_digest,
        } => {
            let _ = (request_id, thread_id, mode, expected_settings_digest);
            "OpenThread"
        }
        SurfaceHostCommand::LoadThread {
            request_id,
            thread_id,
            expected_settings_digest,
            settings_overrides,
            mcp_servers,
        } => {
            let _ = (
                request_id,
                thread_id,
                expected_settings_digest,
                settings_overrides,
                mcp_servers,
            );
            "LoadThread"
        }
        SurfaceHostCommand::ForkThread {
            request_id,
            source_thread_id,
            source_read_token,
            title,
            settings_overrides,
        } => {
            let _ = (
                request_id,
                source_thread_id,
                source_read_token,
                title,
                settings_overrides,
            );
            "ForkThread"
        }
        SurfaceHostCommand::ResolveRunningThread {
            request_id,
            thread_id,
            mode,
        } => {
            let _ = (request_id, thread_id, mode);
            "ResolveRunningThread"
        }
        SurfaceHostCommand::ResumeLatestActiveGoal {
            request_id,
            expected_goal_store_revision,
        } => {
            let _ = (request_id, expected_goal_store_revision);
            "ResumeLatestActiveGoal"
        }
        SurfaceHostCommand::UpdateSessionMetadata {
            request_id,
            thread_id,
            precondition,
            patch,
        } => {
            let _ = (request_id, thread_id, precondition, patch);
            "UpdateSessionMetadata"
        }
        SurfaceHostCommand::QueryInputCatalog {
            request_id,
            context,
            expected_revision,
            query,
        } => {
            let _ = (request_id, context, expected_revision, query);
            "QueryInputCatalog"
        }
        SurfaceHostCommand::ControlJsonlTurn {
            request_id,
            expected_thread_id,
            legacy_turn_id,
            action,
        } => {
            let _ = (request_id, expected_thread_id, legacy_turn_id, action);
            "ControlJsonlTurn"
        }
        SurfaceHostCommand::RememberMemory {
            request_id,
            scope,
            note,
            pin_to_thread,
        } => {
            let _ = (request_id, scope, note, pin_to_thread);
            "RememberMemory"
        }
        SurfaceHostCommand::ReconcileMemoryMutation { token } => {
            let _ = token;
            "ReconcileMemoryMutation"
        }
        SurfaceHostCommand::ReadFolderTrust { request_id, path } => {
            let _ = (request_id, path);
            "ReadFolderTrust"
        }
        SurfaceHostCommand::SetFolderTrust {
            request_id,
            path,
            expected_trust_revision,
            level,
        } => {
            let _ = (request_id, path, expected_trust_revision, level);
            "SetFolderTrust"
        }
        SurfaceHostCommand::ReconcileFolderTrustRevocation { token } => {
            let _ = token;
            "ReconcileFolderTrustRevocation"
        }
        SurfaceHostCommand::ReadRuntimeSettings {
            request_id,
            thread_id,
        } => {
            let _ = (request_id, thread_id);
            "ReadRuntimeSettings"
        }
        SurfaceHostCommand::UpdateRuntimeSettings {
            request_id,
            target,
            expected,
            patch,
        } => {
            let _ = (request_id, target, expected, patch);
            "UpdateRuntimeSettings"
        }
        SurfaceHostCommand::ReconcileHostMutation { token } => {
            let _ = token;
            "ReconcileHostMutation"
        }
        SurfaceHostCommand::CloseThread {
            request_id,
            thread_id,
            expected_owner_epoch,
        } => {
            let _ = (request_id, thread_id, expected_owner_epoch);
            "CloseThread"
        }
        SurfaceHostCommand::ShutdownHost {
            request_id,
            host_incarnation,
        } => {
            let _ = (request_id, host_incarnation);
            "ShutdownHost"
        }
    }
}

#[test]
fn every_manifest_command_inventory_has_an_exhaustive_rust_match() {
    let manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../../../docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json"
    ))
    .unwrap();
    let inventory = &manifest["closed_inventory"];
    let thread_commands = inventory["surface_commands"].as_array().unwrap();
    let host_commands = inventory["surface_host_commands"].as_array().unwrap();

    let expected_thread = [
        "ReserveOperation",
        "AdmitReserved",
        "CancelOperation",
        "CancelSessionCurrent",
        "InterruptGeneration",
        "PauseGoalOperation",
        "ResumeOperation",
        "SteerOperation",
        "TransferBackground",
        "RespondInteraction",
        "ReconcileMutation",
        "RetryStartCommit",
        "RetryProjection",
        "RetryFinalization",
        "ManualCompact",
        "Backtrack",
        "TaskControl",
        "WorkflowControl",
        "GoalMutation",
        "SettingsMutation",
        "McpCatalogQuery",
        "PinnedContextMutation",
    ];
    let expected_host = [
        "ListSessions",
        "SearchSessions",
        "ReadSessionMetadata",
        "ReadSession",
        "ReadThreadPage",
        "CreateThread",
        "OpenThread",
        "LoadThread",
        "ForkThread",
        "ResolveRunningThread",
        "ResumeLatestActiveGoal",
        "UpdateSessionMetadata",
        "QueryInputCatalog",
        "ControlJsonlTurn",
        "RememberMemory",
        "ReconcileMemoryMutation",
        "ReadFolderTrust",
        "SetFolderTrust",
        "ReconcileFolderTrustRevocation",
        "ReadRuntimeSettings",
        "UpdateRuntimeSettings",
        "ReconcileHostMutation",
        "CloseThread",
        "ShutdownHost",
    ];

    assert_eq!(thread_commands.len(), 22);
    assert_eq!(host_commands.len(), 24);
    for (manifest_name, expected_name) in thread_commands.iter().zip(expected_thread) {
        assert_eq!(manifest_name.as_str(), Some(expected_name));
    }
    for (manifest_name, expected_name) in host_commands.iter().zip(expected_host) {
        assert_eq!(manifest_name.as_str(), Some(expected_name));
    }

    let _thread_matcher: fn(&SurfaceCommand) -> &'static str = surface_command_name;
    let _host_matcher: fn(&SurfaceHostCommand) -> &'static str = surface_host_command_name;
}

fn mutation_target_name(value: &MutationTarget) -> &'static str {
    match value {
        MutationTarget::Host { .. } => "Host",
        MutationTarget::Thread { .. } => "Thread",
        MutationTarget::Operation { .. } => "Operation",
        MutationTarget::Generation { .. } => "Generation",
        MutationTarget::Interaction { .. } => "Interaction",
        MutationTarget::Goal { .. } => "Goal",
        MutationTarget::Task { .. } => "Task",
        MutationTarget::Workflow { .. } => "Workflow",
        MutationTarget::Memory { .. } => "Memory",
        MutationTarget::FolderTrust { .. } => "FolderTrust",
        MutationTarget::RuntimeSettings { .. } => "RuntimeSettings",
        MutationTarget::SessionCatalog { .. } => "SessionCatalog",
        MutationTarget::SessionMetadata { .. } => "SessionMetadata",
    }
}

fn deferred_repair_name(value: &DeferredRepair) -> &'static str {
    fn mutation_state(_: &MutationDegradedState) {}
    fn local_projection_state(_: &ProjectionDegradedState) {}
    fn terminal_projection_state(_: &TerminalProjectionDeferredState) {}
    fn remote_owner_state(_: &OwnerAckPendingState) {}
    fn start_state(_: &StartCommitDegradedState) {}
    fn finalization_state(_: &MissingFinalizationDeferredState) {}
    fn memory_pin_state(_: &MemoryPinPendingState) {}
    fn policy_state(_: &PolicyRevocationPendingState) {}
    fn shutdown_state(_: &ShutdownDeferredState) {}
    fn host_settlement_token(_: &ReconcileHostSettlementToken) {}
    fn shutdown_token(_: &ReconcileShutdownToken) {}
    fn local_projection_token(_: &RetryLocalProjectionToken) {}
    fn remote_projection_token(_: &RetryRemoteProjectionToken) {}

    match value {
        DeferredRepair::ThreadMutation { state, .. } => {
            mutation_state(state);
            "ThreadMutation"
        }
        DeferredRepair::HostMutation { state, token } => {
            mutation_state(state);
            host_settlement_token(token);
            "HostMutation"
        }
        DeferredRepair::Projection { state, token } => {
            local_projection_state(state);
            local_projection_token(token);
            "Projection"
        }
        DeferredRepair::TerminalProjection { state, token } => {
            terminal_projection_state(state);
            local_projection_token(token);
            "TerminalProjection"
        }
        DeferredRepair::RemoteOwner { state, token } => {
            remote_owner_state(state);
            remote_projection_token(token);
            "RemoteOwner"
        }
        DeferredRepair::Start { state, .. } => {
            start_state(state);
            "Start"
        }
        DeferredRepair::Finalization { state, .. } => {
            finalization_state(state);
            "Finalization"
        }
        DeferredRepair::MemoryPin { state, .. } => {
            memory_pin_state(state);
            "MemoryPin"
        }
        DeferredRepair::Policy { state, .. } => {
            policy_state(state);
            "Policy"
        }
        DeferredRepair::Shutdown { state, token } => {
            shutdown_state(state);
            shutdown_token(token);
            "Shutdown"
        }
    }
}

fn mutation_result_name(value: &RuntimeSurfaceMutationResult) -> &'static str {
    match value {
        RuntimeSurfaceMutationResult::Committed(_) => "Committed",
        RuntimeSurfaceMutationResult::Deferred(_) => "Deferred",
        RuntimeSurfaceMutationResult::Uncommitted(_) => "Uncommitted",
    }
}

fn mutation_ack_requirement_name(value: &MutationAckRequirement) -> &'static str {
    match value {
        MutationAckRequirement::ThreadCursor(value) => {
            let _: &ThreadCursorAckRequirement = value;
            "ThreadCursor"
        }
        MutationAckRequirement::ThreadRemoteOwner { .. } => "ThreadRemoteOwner",
        MutationAckRequirement::HostReceipt(value) => {
            let _: &HostReceiptAckRequirement = value;
            "HostReceipt"
        }
        MutationAckRequirement::GoalStoreReceipt { .. } => "GoalStoreReceipt",
        MutationAckRequirement::OperationTerminal(value) => {
            let _: &OperationTerminalAckRequirement = value;
            "OperationTerminal"
        }
        MutationAckRequirement::PolicyRevocationBarrier { .. } => "PolicyRevocationBarrier",
    }
}

fn shutdown_plan_refinements_are_exact(value: &ShutdownBarrierPlan) {
    fn host(_: &HostReceiptAckRequirement) {}
    fn thread_plan(value: &ShutdownThreadPlan) {
        fn cursor(_: &ThreadCursorAckRequirement) {}
        fn host(_: &HostReceiptAckRequirement) {}
        match value {
            ShutdownThreadPlan::Recorded {
                session_closed,
                catalog_closed,
                ..
            } => {
                cursor(session_closed);
                host(catalog_closed);
            }
            ShutdownThreadPlan::Ephemeral { session_closed, .. } => cursor(session_closed),
        }
    }

    match value {
        ShutdownBarrierPlan::CloseThread { thread, .. } => thread_plan(thread),
        ShutdownBarrierPlan::ShutdownHost {
            threads,
            final_host_lifecycle,
            ..
        } => {
            for thread in threads {
                thread_plan(thread);
            }
            host(final_host_lifecycle);
        }
    }
}

fn host_receipt_identities_are_closed(requirement: &HostReceiptAckRequirement) {
    let _: &HostReceiptRequirementIdentity = &requirement.identity;
}

fn host_commit_ack_identity_is_closed(value: &MutationCommitAck) {
    if let MutationCommitAck::HostCommitAck { identity, .. } = value {
        let _: &HostReceiptIdentityPair = identity;
    }
}

fn mutation_reply_name<T>(value: &MutationReply<T>) -> &'static str {
    match value {
        MutationReply::Committed { mutation, value } => {
            let _ = (mutation, value);
            "Committed"
        }
        MutationReply::Deferred { mutation, partial } => {
            let _ = (mutation, partial);
            "Deferred"
        }
        MutationReply::Uncommitted { mutation } => {
            let _ = mutation;
            "Uncommitted"
        }
    }
}

#[test]
fn mutation_and_repair_algebras_remain_closed() {
    let _target_matcher: fn(&MutationTarget) -> &'static str = mutation_target_name;
    let _repair_matcher: fn(&DeferredRepair) -> &'static str = deferred_repair_name;
    let _result_matcher: fn(&RuntimeSurfaceMutationResult) -> &'static str = mutation_result_name;
    let _reply_matcher: fn(&MutationReply<()>) -> &'static str = mutation_reply_name;
    let _ack_matcher: fn(&MutationAckRequirement) -> &'static str = mutation_ack_requirement_name;
    let _shutdown_refinement_matcher: fn(&ShutdownBarrierPlan) =
        shutdown_plan_refinements_are_exact;
    let _host_requirement_matcher: fn(&HostReceiptAckRequirement) =
        host_receipt_identities_are_closed;
    let _host_ack_matcher: fn(&MutationCommitAck) = host_commit_ack_identity_is_closed;

    let _thread_repair: Option<ReconcileMutationToken> = None;
    let _start_repair: Option<RetryStartCommitToken> = None;
    let _projection_repair: Option<RetryProjectionToken> = None;
    let _finalization_repair: Option<RetryFinalizationToken> = None;
    let _host_repair: Option<ReconcileHostMutationToken> = None;
    let _memory_repair: Option<ReconcileMemoryMutationToken> = None;
    let _policy_repair: Option<ReconcileFolderTrustRevocationToken> = None;
}

fn attach_result_name(value: &AttachResult) -> &'static str {
    match value {
        AttachResult::FreshAttached { .. } => "FreshAttached",
        AttachResult::CursorAttached { .. } => "CursorAttached",
        AttachResult::Denied { .. } => "Denied",
        AttachResult::SnapshotRequired { .. } => "SnapshotRequired",
        AttachResult::InvalidCursor { .. } => "InvalidCursor",
        AttachResult::ThreadClosed { .. } => "ThreadClosed",
        AttachResult::Unavailable { .. } => "Unavailable",
    }
}

fn subscription_item_name(value: &SurfaceSubscriptionItem) -> &'static str {
    match value {
        SurfaceSubscriptionItem::Batch { .. } => "Batch",
        SurfaceSubscriptionItem::Gap { .. } => "Gap",
        SurfaceSubscriptionItem::Sealed { .. } => "Sealed",
    }
}

fn detach_result_name(value: &DetachResult) -> &'static str {
    match value {
        DetachResult::Detached { .. } => "Detached",
        DetachResult::AlreadyDetached { .. } => "AlreadyDetached",
        DetachResult::Deferred { .. } => "Deferred",
        DetachResult::StaleAttachment { .. } => "StaleAttachment",
    }
}

fn wait_result_name(value: &WaitOperationTerminalResult) -> &'static str {
    match value {
        WaitOperationTerminalResult::Terminal { .. } => "Terminal",
        WaitOperationTerminalResult::TerminalCommitFailure { .. } => "TerminalCommitFailure",
        WaitOperationTerminalResult::TerminalProjectionFailure { .. } => {
            "TerminalProjectionFailure"
        }
        WaitOperationTerminalResult::UnknownOperation { .. } => "UnknownOperation",
        WaitOperationTerminalResult::WrongThread { .. } => "WrongThread",
        WaitOperationTerminalResult::WaitCancelled { .. } => "WaitCancelled",
    }
}

fn read_result_name<T>(value: &SurfaceReadResult<T>) -> &'static str {
    match value {
        SurfaceReadResult::Found {
            request_id,
            revision,
            value,
        } => {
            let _ = (request_id, revision, value);
            "Found"
        }
        SurfaceReadResult::NotFound { request_id, error } => {
            let _ = (request_id, error);
            "NotFound"
        }
        SurfaceReadResult::Invalid { request_id, error } => {
            let _ = (request_id, error);
            "Invalid"
        }
        SurfaceReadResult::Stale { request_id, error } => {
            let _ = (request_id, error);
            "Stale"
        }
        SurfaceReadResult::Unavailable { request_id, error } => {
            let _ = (request_id, error);
            "Unavailable"
        }
    }
}

fn surface_event_name(value: &SurfaceEvent) -> &'static str {
    match value {
        SurfaceEvent::Operation(_) => "Operation",
        SurfaceEvent::Item(_) => "Item",
        SurfaceEvent::Assistant(_) => "Assistant",
        SurfaceEvent::Tool(_) => "Tool",
        SurfaceEvent::Plan(_) => "Plan",
        SurfaceEvent::Usage(_) => "Usage",
        SurfaceEvent::Context(_) => "Context",
        SurfaceEvent::Interaction(_) => "Interaction",
        SurfaceEvent::Task(_) => "Task",
        SurfaceEvent::Workflow(_) => "Workflow",
        SurfaceEvent::Subagent(_) => "Subagent",
        SurfaceEvent::Goal(_) => "Goal",
        SurfaceEvent::Settings(_) => "Settings",
        SurfaceEvent::McpCatalog(_) => "McpCatalog",
        SurfaceEvent::PinnedContext(_) => "PinnedContext",
        SurfaceEvent::Session(_) => "Session",
    }
}

#[test]
fn snapshot_attach_detach_wait_event_and_read_surfaces_are_closed() {
    fn assert_clone<T: Clone>() {}

    assert_clone::<SurfaceSnapshot>();
    assert_clone::<SnapshotAtCursor>();
    assert_clone::<FreshAttachRequest>();
    assert_clone::<CursorAttachRequest>();
    assert_clone::<WaitOperationTerminalRequest>();
    assert_clone::<SurfaceCommitBatch>();

    let _attach_matcher: fn(&AttachResult) -> &'static str = attach_result_name;
    let _subscription_matcher: fn(&SurfaceSubscriptionItem) -> &'static str =
        subscription_item_name;
    let _detach_matcher: fn(&DetachResult) -> &'static str = detach_result_name;
    let _wait_matcher: fn(&WaitOperationTerminalResult) -> &'static str = wait_result_name;
    let _read_matcher: fn(&SurfaceReadResult<()>) -> &'static str = read_result_name;
    let _event_matcher: fn(&SurfaceEvent) -> &'static str = surface_event_name;
}

#[test]
fn fixed_surface_budgets_and_page_limit_constructors_enforce_bounds() {
    assert_eq!(SURFACE_RESERVATION_LEASE_MS, 30_000);
    assert_eq!(SURFACE_COMMIT_BATCH_EVENT_LIMIT, 1_024);
    assert_eq!(SURFACE_COMMIT_BATCH_BYTE_LIMIT, 8_388_608);
    assert_eq!(SURFACE_RETAINED_EVENT_LIMIT, 8_192);
    assert_eq!(SURFACE_RETAINED_BYTE_LIMIT, 33_554_432);
    assert_eq!(SURFACE_SUBSCRIBER_EVENT_LIMIT, 1_024);
    assert_eq!(SURFACE_SUBSCRIBER_BYTE_LIMIT, 8_388_608);
    assert_eq!(ACP_MAX_INBOUND_LINE_BYTES, 8_388_608);
    assert_eq!(ACP_MAX_OUTBOUND_FRAME_BYTES, 8_388_608);
    assert_eq!(ACP_INGRESS_MESSAGE_LIMIT, 64);
    assert_eq!(ACP_INGRESS_BYTE_LIMIT, 16_777_216);
    assert_eq!(ACP_OUTGOING_MESSAGE_LIMIT, 256);
    assert_eq!(ACP_OUTGOING_BYTE_LIMIT, 33_554_432);
    assert_eq!(ACP_LOAD_GATE_MESSAGE_LIMIT, 4_096);
    assert_eq!(ACP_LOAD_GATE_BYTE_LIMIT, 67_108_864);
    assert_eq!(ACP_PROMPT_GATE_MESSAGE_LIMIT, 1_024);
    assert_eq!(ACP_PROMPT_GATE_BYTE_LIMIT, 16_777_216);
    assert_eq!(ACP_WRITE_FLUSH_DEADLINE_MS, 30_000);
    assert_eq!(ACP_REVERSE_REQUEST_DEADLINE_MS, 120_000);
    assert_eq!(ACP_CAPABILITY_CALL_DEADLINE_MS, 60_000);
    assert_eq!(ACP_CAPABILITY_RESULT_CANONICAL_BYTE_LIMIT, 4_194_304);
    assert_eq!(ACP_CAPABILITY_TEXT_BYTE_LIMIT, 4_194_304);
    assert_eq!(ACP_CAPABILITY_IDENTIFIER_BYTE_LIMIT, 4_096);
    assert_eq!(ACP_TERMINAL_KILL_DEADLINE_MS, 10_000);
    assert_eq!(ACP_TERMINAL_RELEASE_DEADLINE_MS, 10_000);
    assert_eq!(ACP_SUPERVISOR_JOIN_DEADLINE_MS, 5_000);
    assert_eq!(ACP_TOMBSTONE_TTL_MS, 300_000);
    assert_eq!(ACP_TOMBSTONE_LIMIT, 4_096);
    assert_eq!(JSONL_REQUEST_TOMBSTONE_TTL_MS, 300_000);
    assert_eq!(JSONL_REQUEST_TOMBSTONE_LIMIT, 4_096);
    assert_eq!(JSONL_LIVE_REQUEST_LIMIT, 1_024);
    assert_eq!(JSONL_REPAIR_AUTHORITY_LIMIT, 1_024);
    assert_eq!(JSONL_COMMITTED_REPAIR_DRAIN_DEADLINE_MS, 5_000);
    assert_eq!(JSONL_SUPERVISOR_JOIN_DEADLINE_MS, 5_000);

    assert!(SurfacePageLimit::try_session_catalog(0).is_err());
    assert!(SurfacePageLimit::try_session_catalog(1).is_ok());
    assert!(SurfacePageLimit::try_session_catalog(100).is_ok());
    assert!(SurfacePageLimit::try_session_catalog(101).is_err());
    assert!(SurfacePageLimit::try_thread_page(0).is_err());
    assert!(SurfacePageLimit::try_thread_page(500).is_ok());
    assert!(SurfacePageLimit::try_thread_page(501).is_err());
    assert!(matches!(
        SurfacePageLimit::legacy_jsonl(0),
        SurfacePageLimit::LegacyJsonl { wire_value: 0, effective } if effective.get() == 1
    ));
}

#[test]
fn every_identity_revision_fence_scope_and_capability_is_constructible() {
    macro_rules! uuid_v7_ids {
        ($($ty:ty),+ $(,)?) => {{
            let mut seed = 1_u8;
            $(
                let _: $ty = <$ty>::try_from_bytes(uuid_v7_bytes(seed)).unwrap();
                seed = seed.wrapping_add(1);
            )+
            assert_ne!(seed, 1);
        }};
    }
    uuid_v7_ids!(
        SurfaceOperationId,
        SurfaceStreamId,
        SurfaceInteractionId,
        SurfaceAttachmentId,
        SurfaceResponseId,
        SurfaceResponseReceiptId,
        SurfaceEventId,
        SurfaceRequestId,
        SurfaceCommitId,
        SurfaceSettlementId,
        SurfaceFinalizeIntentId,
        SurfaceAdmissionLeaseId,
        SurfaceInputCorrelationId,
        SurfaceCapabilityCallId,
        SurfaceConnectionId,
        HostIncarnation,
    );

    macro_rules! text_ids {
        ($($ty:ty),+ $(,)?) => {$({ let _: $ty = <$ty>::try_new("released-id").unwrap(); })+};
    }
    text_ids!(
        SurfaceToolCallId,
        SurfaceTaskId,
        SurfaceWorkflowRunId,
        SurfaceWorkflowResultId,
        SurfaceSubagentId,
        SurfaceGoalId,
        SurfaceGoalRunId,
        SurfaceGoalOuterTurnId,
        SurfaceGoalIntentId,
        SurfaceRemoteTerminalId,
        SurfaceCatalogEntryId,
    );

    let thread_id = SurfaceThreadId::try_from_bytes([2; 16]).unwrap();
    let operation_id = SurfaceOperationId::try_from_bytes(uuid_v7_bytes(2)).unwrap();
    let generation_id = SurfaceGenerationId::new(0);
    let fence = SurfaceOperationFence {
        thread_id: thread_id.clone(),
        thread_owner_epoch: ThreadOwnerEpoch::new(0),
        operation_id: operation_id.clone(),
        generation_id,
    };
    let goal_fence = SurfaceGoalFence {
        goal_id: SurfaceGoalId::try_new("goal-1").unwrap(),
        goal_revision: GoalRevision::try_new(1).unwrap(),
        goal_owner_epoch: GoalOwnerEpoch::try_new(1).unwrap(),
    };
    let _task_fence = SurfaceTaskFence {
        task_id: SurfaceTaskId::try_new("task-1").unwrap(),
        task_revision: TaskRevision::try_new(1).unwrap(),
        background_owner: None,
    };
    let _workflow_fence = SurfaceWorkflowFence {
        workflow_run_id: SurfaceWorkflowRunId::try_new("workflow-1").unwrap(),
        workflow_revision: WorkflowRevision::try_new(1).unwrap(),
        parent: Some(fence.clone()),
    };

    let scopes = [
        SurfaceScope::Thread,
        SurfaceScope::Operation { operation_id },
        SurfaceScope::Generation {
            fence: fence.clone(),
        },
        SurfaceScope::Goal {
            goal_id: goal_fence.goal_id,
            causative_generation: Some(fence.clone()),
        },
    ];
    for scope in scopes {
        match scope {
            SurfaceScope::Thread
            | SurfaceScope::Operation { .. }
            | SurfaceScope::Generation { .. }
            | SurfaceScope::Background { .. }
            | SurfaceScope::Goal { .. } => {}
        }
    }

    let commit_id = SurfaceCommitId::try_from_bytes(uuid_v7_bytes(3)).unwrap();
    let recorded = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(0),
        durable_revision: DurableRevision::try_new(1).unwrap(),
        commit_id: commit_id.clone(),
    };
    let ephemeral = CommitClass::Ephemeral {
        incarnation: SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(4)).unwrap(),
        live_revision: LiveRevision::try_new(1).unwrap(),
        commit_id,
    };
    for commit in [recorded, ephemeral] {
        match commit {
            CommitClass::Recorded { .. } | CommitClass::Ephemeral { .. } => {}
        }
    }

    let capabilities = [
        SurfaceCapability::ReadSnapshot,
        SurfaceCapability::ReadCatalog,
        SurfaceCapability::SubmitOperation,
        SurfaceCapability::ControlBoundOperation,
        SurfaceCapability::ControlAnyVisibleOperation,
        SurfaceCapability::LegacyCancelCurrent,
        SurfaceCapability::LegacyInterruptResume,
        SurfaceCapability::LegacyJsonlControl,
        SurfaceCapability::RespondGrantedInteraction,
        SurfaceCapability::ManageTask,
        SurfaceCapability::ManageWorkflow,
        SurfaceCapability::ManageGoal,
        SurfaceCapability::ManageThreadSettings,
        SurfaceCapability::ManagePinnedContext,
        SurfaceCapability::RepairThread,
        SurfaceCapability::ReadSessionCatalog,
        SurfaceCapability::ManageSessionCatalog,
        SurfaceCapability::ManageSessionLifecycle,
        SurfaceCapability::ManageMemory,
        SurfaceCapability::ReadHostPolicy,
        SurfaceCapability::ManageFolderTrust,
        SurfaceCapability::ReadHostSettings,
        SurfaceCapability::ManageHostSettings,
        SurfaceCapability::ShutdownHost,
    ];
    assert_eq!(capabilities.len(), 24);

    let _released_turn: SurfaceTurnId = SurfaceTurnId::new();
    let _released_item: SurfaceItemId = SurfaceItemId::new();
}

#[test]
fn independent_revision_domains_and_scalar_wrappers_do_not_collapse() {
    macro_rules! revisions {
        ($($ty:ty),+ $(,)?) => {$({ let _: $ty = <$ty>::try_new(1).unwrap(); })+};
    }
    revisions!(
        DurableRevision,
        LiveRevision,
        SessionCatalogRevision,
        McpCatalogRevision,
        InputCatalogRevision,
        WorkflowCatalogRevision,
        SessionMetadataRevision,
        SettingsRevision,
        TrustRevision,
        PolicyEpoch,
        MemoryRevision,
        PinnedContextRevision,
        SessionHealthRevision,
        GoalRevision,
        GoalCatalogRevision,
        GoalOwnerEpoch,
        TaskRevision,
        WorkflowRevision,
        SubagentRevision,
        InteractionRevision,
        ResponseRouteEpoch,
        CapabilityRevision,
        PlanRevision,
        UsageRevision,
        ContextRevision,
        PinnedFileRevision,
        PinnedUserRevision,
        PinnedSystemRevision,
        ProjectRootMemoryRevision,
        BootstrapCredentialRevision,
        HostLifecycleRevision,
    );
    let _ = GoalObjectiveRevision::new(0);
    let _ = ThreadOwnerEpoch::new(0);
    let _ = SurfaceGenerationId::new(0);
    let _ = UnixMillis::new(-1);
    let _ = DurationMillis::new(0);
    let _ = MonotonicTick::new(0);
    let _ = ByteOffset::new(0);
    let _ = ByteCount::new(0);
    let _ = SequenceNumber::new(0);
    let _ = Sha256Digest::new([0; 32]);

    let clock_id = HostMonotonicClockId::try_from_bytes(uuid_v7_bytes(5)).unwrap();
    let _instant = MonotonicInstant {
        clock_id,
        tick: MonotonicTick::new(7),
    };
    let _cursor = SurfaceCursor {
        thread_id: SurfaceThreadId::try_from_bytes([6; 16]).unwrap(),
        incarnation: SurfaceIncarnation::try_from_bytes(uuid_v7_bytes(6)).unwrap(),
        next_seq: SequenceNumber::new(0),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(1).unwrap(),
        },
    };

    let _ = PinnedContextSourceRevision::Memory(MemoryRevision::try_new(1).unwrap());
    let _ = PinnedContextSourceRevision::File(PinnedFileRevision::try_new(1).unwrap());
    let _ = PinnedContextSourceRevision::User(PinnedUserRevision::try_new(1).unwrap());
    let _ = PinnedContextSourceRevision::System(PinnedSystemRevision::try_new(1).unwrap());
    let witnesses = [
        HostRevisionWitness::Memory(MemoryRevision::try_new(1).unwrap()),
        HostRevisionWitness::FolderTrust(TrustRevision::try_new(1).unwrap()),
        HostRevisionWitness::RuntimeSettings(SettingsRevision::try_new(1).unwrap()),
        HostRevisionWitness::SessionCatalog(SessionCatalogRevision::try_new(1).unwrap()),
        HostRevisionWitness::SessionMetadata(SessionMetadataRevision::try_new(1).unwrap()),
        HostRevisionWitness::HostLifecycle(HostLifecycleRevision::try_new(1).unwrap()),
    ];
    assert_eq!(witnesses.len(), 6);

    let _ = AcpRequestId::String(NonEmptyText::try_new("rpc").unwrap());
    let _ = AcpRequestId::Integer(-1);
    for reason in [
        SurfaceUnavailableReason::HostShuttingDown,
        SurfaceUnavailableReason::ThreadClosing,
        SurfaceUnavailableReason::ProjectionDegraded,
        SurfaceUnavailableReason::CapacityExceeded,
        SurfaceUnavailableReason::RuntimeUnavailable,
    ] {
        match reason {
            SurfaceUnavailableReason::HostShuttingDown
            | SurfaceUnavailableReason::ThreadClosing
            | SurfaceUnavailableReason::ProjectionDegraded
            | SurfaceUnavailableReason::CapacityExceeded
            | SurfaceUnavailableReason::RuntimeUnavailable => {}
        }
    }
}
