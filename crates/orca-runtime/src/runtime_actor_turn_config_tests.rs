// Actor-level regressions: keep the actor catalog distinct from the executing turn.
fn turn_config_actor(cwd: &std::path::Path) -> ThreadActor {
    let config = surface_test_config(cwd.to_path_buf(), HistoryMode::Record);
    let mut prepared = prepare_and_start_runtime_thread(RuntimeThreadStartRequest::new(
        config.clone(),
        "turn tool catalog",
    ))
    .unwrap();
    let (command_tx, _command_rx) = tokio_mpsc::channel(THREAD_COMMAND_CAPACITY);
    let (capability_tx, _capability_rx) = tokio_mpsc::channel(1);
    let (surface_handle, resident) = bootstrap_runtime_surface(
        &mut prepared.thread,
        &config,
        "turn tool catalog",
        surface::HostIncarnation::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap(),
        command_tx.clone(),
        capability_tx,
        prepared.surface_owner.take().unwrap(),
        surface::SurfaceHubConfig::default(),
    )
    .unwrap();
    let mcp = prepared.thread.session().mcp_registry().clone();
    let handle = RuntimeThreadHandle {
        thread_id: prepared.thread.thread_id().to_string(),
        session_id: prepared.thread.session().session_id().map(str::to_string),
        parent_thread_id: None,
        startup_warnings: Arc::new(Vec::new()),
        task_registry: prepared.thread.session().task_registry().clone(),
        mcp_registry: mcp.clone(),
        mcp_registry_owned: Arc::new(AtomicBool::new(false)),
        mcp_startup: crate::mcp_startup::McpStartupWarnings::watch(&mcp),
        command_tx,
        surface: surface_handle,
        prompt_queue_updates: tokio::sync::watch::channel(
            crate::prompt_queue::PromptQueueSnapshot::default(),
        )
        .0,
        prompt_queue_observer: Arc::new(Mutex::new(None)),
        prompt_queue_interaction_handlers: Arc::new(Mutex::new(
            PromptQueueInteractionHandlers::default(),
        )),
        prompt_queue_dispatch_ready: Arc::new(AtomicBool::new(false)),
        prompt_queue_surface_attached: Arc::new(AtomicBool::new(false)),
    };
    ThreadActor::new(
        prepared.thread,
        config,
        handle,
        Arc::new(OutputWriterExecutor),
        4,
        Some(resident),
        Duration::from_secs(30),
        0,
    )
}

fn admit_turn_config_operation(actor: &mut ThreadActor, action: ActionKind) -> ActiveOperation {
    let attachment = fresh_surface_attachment(&actor.handle.surface());
    let intent = surface_user_turn_intent(&attachment.baseline.snapshot, "override");
    admit_turn_config_operation_with_attachment(actor, action, &attachment, intent)
}

fn admit_turn_config_operation_with_attachment(
    actor: &mut ThreadActor,
    action: ActionKind,
    attachment: &surface::FreshSurfaceAttachment,
    intent: surface::OperationRequestIntent,
) -> ActiveOperation {
    let reserved = committed_surface_value(
        actor
            .reserve_surface_operation(
                surface_request_id(),
                intent,
                attachment.client.attachment_id().clone(),
                attachment.client.connection_id().cloned(),
            )
            .unwrap(),
    );
    let (prepared, goal) = actor
        .prepare_surface_admission(
            &attachment.client,
            surface_request_id(),
            reserved.operation_id,
            reserved.lease.lease_id,
            None,
        )
        .unwrap();
    assert!(goal.is_none());
    committed_surface_value(
        actor
            .finish_prepared_surface_admission(prepared, None)
            .unwrap(),
    );
    let mut active = actor.active.take().unwrap();
    active.config.external_tools = vec![orca_core::external_config::ExternalToolConfig {
        name: "turn_tool".to_string(),
        description: "Only advertised in this turn".to_string(),
        action_kind: action,
        command: "unused".to_string(),
        schema: serde_json::json!({"type": "object", "properties": {}}),
    }];
    assert!(actor.config.external_tools.is_empty());
    active
}

fn turn_config_response(active: &ActiveOperation) -> RuntimeModelResponse {
    let tool_name = &active.config.external_tools[0].name;
    RuntimeModelResponse::new(
        ProviderResponse {
            steps: vec![ProviderStep::ToolCall(ToolRequest {
                id: "turn-tool-call".into(),
                name: ToolName::from_str(tool_name).unwrap(),
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".into()),
            })],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: vec![orca_core::conversation::RawToolCall {
                id: "turn-tool-call".to_string(),
                function_name: tool_name.clone(),
                arguments: "{}".to_string(),
            }],
            usage: None,
        },
        active.request.turn_id().clone(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn foreground_response_classifies_external_tools_with_turn_override() {
    for (action, expected) in [
        (ActionKind::Write, surface::SurfaceToolAction::Write),
        (ActionKind::Shell, surface::SurfaceToolAction::Shell),
    ] {
        let cwd = tempfile::tempdir().unwrap();
        let mut actor = turn_config_actor(cwd.path());
        let mut active = admit_turn_config_operation(&mut actor, action);
        let response = turn_config_response(&active);
        let fence = active.surface_operation.clone().unwrap();
        actor
            .commit_surface_provider_response(&mut active, fence, &response)
            .unwrap();
        let snapshot = actor.resident_surface.coordinator.state().snapshot();
        assert_eq!(snapshot.tools.last().unwrap().request.action, expected);
        active.generation.join.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_approval_validates_external_tools_with_turn_override() {
    for (action, expected) in [
        (ActionKind::Write, surface::SurfaceToolAction::Write),
        (ActionKind::Shell, surface::SurfaceToolAction::Shell),
    ] {
        let cwd = tempfile::tempdir().unwrap();
        let mut actor = turn_config_actor(cwd.path());
        let attachment = fresh_surface_goal_approval_attachment(&actor.handle.surface());
        let _subscription = actor
            .handle
            .surface()
            .claim_subscription(&attachment.subscription)
            .unwrap();
        let intent = surface_user_turn_intent(&attachment.baseline.snapshot, "approve override");
        let mut active =
            admit_turn_config_operation_with_attachment(&mut actor, action, &attachment, intent);
        let response = turn_config_response(&active);
        let fence = active.surface_operation.clone().unwrap();
        let response = actor
            .commit_surface_provider_response(&mut active, fence.clone(), &response)
            .unwrap();
        let ProviderStep::ToolCall(request) = &response.response.steps[0] else {
            panic!("expected provider tool call");
        };
        let (reply, resolution) = mpsc::sync_channel(1);
        actor.request_surface_tool_approval(
            &mut active,
            fence,
            orca_core::approval_types::ApprovalRequest {
                id: "turn-tool-approval".into(),
                action,
                description: "Approve turn-only external tool".into(),
                tool: Some("turn_tool".into()),
                target: None,
                preview: None,
            },
            request.clone(),
            reply,
        );
        match resolution.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            result => panic!("approval should wait for an answer, got {result:?}"),
        }
        let interaction = actor
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .interactions
            .last()
            .expect("turn-only tool approval was requested")
            .clone();
        let surface::SurfaceInteractionRequest::ToolApproval { tool, .. } = &interaction.request
        else {
            panic!("expected tool approval");
        };
        assert_eq!(tool.action, expected);
        committed_surface_value(
            actor
                .respond_surface_interaction_by_id(
                    &attachment.client,
                    surface_request_id(),
                    interaction.interaction_id,
                    surface::SurfaceClientInteractionAnswer::ToolApproval {
                        decision: surface::SurfaceAllowDeny::Allow,
                    },
                )
                .unwrap(),
        );
        let resolution = resolution.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap().unwrap();
        assert_eq!(resolution.id, "turn-tool-approval");
        assert_eq!(
            resolution.decision,
            orca_core::approval_types::ApprovalDecision::Allow
        );
        active.generation.join.await.unwrap();
    }
}

// Reproduce the pre-fix durable checkpoint: its fingerprint covered the
// actor config without proving that it matched the executing turn's tool.
fn record_legacy_turn_tool_approval(
    actor: &mut ThreadActor,
    active: &mut ActiveOperation,
    fence: surface::SurfaceOperationFence,
    request: &ToolRequest,
) {
    let snapshot = actor.resident_surface.coordinator.state().snapshot();
    let tool = actor
        .surface_tool_for_runtime_request(snapshot, &fence, request, &active.config.external_tools)
        .unwrap();
    let authority = ThreadActor::surface_authority_for_tool(snapshot, &fence, &tool).unwrap();
    let intent = surface::ToolInvocationIntent::before_invocation(tool.clone(), authority.clone());
    let interaction_request = surface::SurfaceInteractionRequest::ToolApproval {
        tool,
        description: surface::DisplayText::new("legacy turn-tool approval"),
        preview: None,
        authority,
    };
    let interaction_id =
        surface::SurfaceInteractionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap();
    let capsule = surface::DurableInteractionContinuationCapsule::try_new_restartable(
        interaction_id.clone(),
        fence.clone(),
        interaction_request.clone(),
        cold_recovery_thread_config_fingerprint(&actor.config, snapshot),
        surface::DurableInteractionContinuationIntent::ToolInvocation(intent),
    )
    .unwrap();
    let disposition =
        surface::InteractionUnavailableDisposition::restartable_tool_approval(&capsule).unwrap();
    actor
        .commit_surface_effect_interaction_request(
            active,
            fence,
            interaction_id,
            surface::SurfaceInteractionKind::ToolApproval,
            interaction_request,
            disposition,
        )
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_recovery_settles_approval_when_actor_cannot_restore_turn_tool_definition() {
    assert_cold_recovery_turn_tool_approval(&[
        ("turn_tool", "missing"),
        ("turn_tool", "different command"),
        ("turn_tool", "different schema"),
        ("turn_tool", "identical"),
        ("ops__deploy", "missing"),
        ("ops__deploy", "different command"),
        ("mcp__ops__deploy", "missing"),
        ("mcp__ops__deploy", "different command"),
    ])
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_recovery_rejects_legacy_approval_without_turn_tool_definition_proof() {
    assert_cold_recovery_turn_tool_approval(&[("turn_tool", "legacy different command")]).await;
}

async fn assert_cold_recovery_turn_tool_approval(cases: &[(&str, &str)]) {
    let home = tempfile::tempdir().unwrap();
    let _home = crate::history::redirect_test_orca_home(home.path());
    for &(tool_name, actor_definition) in cases {
        let cwd = tempfile::tempdir().unwrap();
        let mut actor = turn_config_actor(cwd.path());
        let attachment = fresh_surface_goal_approval_attachment(&actor.handle.surface());
        let subscription = actor
            .handle
            .surface()
            .claim_subscription(&attachment.subscription)
            .unwrap();
        let intent = surface_user_turn_intent(&attachment.baseline.snapshot, "approve override");
        let mut active = admit_turn_config_operation_with_attachment(
            &mut actor,
            ActionKind::Shell,
            &attachment,
            intent,
        );
        active.config.external_tools[0].name = tool_name.into();
        // Keep the same name and action: those alone cannot prove that cold
        // recovery would run the definition whose approval was requested.
        if actor_definition != "missing" {
            let mut definition = active.config.external_tools[0].clone();
            match actor_definition {
                "different command" | "legacy different command" => {
                    definition.command = "different-command".into()
                }
                "different schema" => {
                    definition.schema = serde_json::json!({"type": "object", "properties": {"other": {"type": "string"}}})
                }
                "identical" => {}
                _ => unreachable!(),
            }
            actor.config.external_tools = vec![definition];
        }
        let mut recovery_config = actor.config.clone();
        let response = turn_config_response(&active);
        let fence = active.surface_operation.clone().unwrap();
        let response = actor
            .commit_surface_provider_response(&mut active, fence.clone(), &response)
            .unwrap();
        let ProviderStep::ToolCall(request) = &response.response.steps[0] else {
            panic!("expected provider tool call");
        };
        let (reply, resolution) = mpsc::sync_channel(1);
        if actor_definition == "legacy different command" {
            record_legacy_turn_tool_approval(&mut actor, &mut active, fence.clone(), request);
        } else {
            actor.request_surface_tool_approval(
                &mut active,
                fence.clone(),
                orca_core::approval_types::ApprovalRequest {
                    id: "turn-tool-approval".into(),
                    action: ActionKind::Shell,
                    description: "Approve executing turn definition".into(),
                    tool: Some(tool_name.into()),
                    target: None,
                    preview: None,
                },
                request.clone(),
                reply,
            );
        }
        assert!(matches!(
            resolution.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let interaction_id = actor
            .resident_surface
            .coordinator
            .state()
            .snapshot()
            .interactions
            .last()
            .unwrap()
            .interaction_id
            .clone();
        recovery_config.history_mode = HistoryMode::Resume(actor.handle.thread_id.clone());
        active.generation.join.await.unwrap();
        // Drop the live owner without answering or committing a terminal;
        // resume the recorded ledger with a new cold owner below.
        drop(subscription);
        drop(attachment);
        drop(actor);
        let host = RuntimeHost::start_with_executor(Arc::new(OutputWriterExecutor)).unwrap();
        let recovered = host
            .start_thread(recovery_config, "recover turn approval")
            .unwrap();
        let snapshot = fresh_surface_attachment(&recovered.surface())
            .baseline
            .snapshot;
        let interaction = snapshot
            .interactions
            .iter()
            .find(|interaction| interaction.interaction_id == interaction_id)
            .unwrap();
        let operation =
            ThreadActor::surface_operation_record(&snapshot, &fence.operation_id).unwrap();
        if actor_definition == "identical" {
            assert!(matches!(
                interaction.lifecycle,
                surface::SurfaceInteractionLifecycle::Requested
            ));
            assert!(operation.terminal.is_none());
        } else {
            assert!(
                matches!(
                    interaction.lifecycle,
                    surface::SurfaceInteractionLifecycle::Cancelled {
                        reason: surface::InteractionCancelReason::CapabilityUnavailable,
                    }
                ),
                "{tool_name} / {actor_definition}: approval must be cancelled after cold recovery"
            );
            assert!(
                matches!(
                    operation
                        .terminal
                        .as_ref()
                        .map(|terminal| &terminal.terminal),
                    Some(surface::OperationTerminal::Failed {
                        class: surface::FailureClass::RuntimeInvariant,
                        ..
                    })
                ),
                "{tool_name} / {actor_definition}: {:?}",
                operation.terminal
            );
        }
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.request.tool_call_id.as_str() == "turn-tool-call")
            .unwrap();
        assert!(
            tool.invocation_started.is_none(),
            "{tool_name} / {actor_definition}: recovered dispatch must not start"
        );
        host.shutdown().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_read_validates_external_tools_with_turn_override() {
    let cwd = tempfile::tempdir().unwrap();
    let mut actor = turn_config_actor(cwd.path());
    let surface = actor
        .handle
        .acp_surface_for_connection(
            surface::SurfaceConnectionId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes()).unwrap(),
        )
        .unwrap();
    let attachment = match surface.attach_acp_fresh(
        surface::FreshAttachRequest {
            request_id: surface_request_id(),
            role: surface::SurfaceAttachmentRole::Acp,
            requested_capabilities: BTreeSet::from([
                surface::SurfaceCapability::ReadSnapshot,
                surface::SurfaceCapability::SubmitOperation,
                surface::SurfaceCapability::ControlBoundOperation,
            ]),
            interaction_capabilities: BTreeSet::new(),
        },
        surface::AcpAttachmentCapabilityProfile {
            revision: surface::CapabilityRevision::try_new(1).unwrap(),
            standard: surface::AcpStandardCapabilitySet {
                file_read: true,
                ..surface::AcpStandardCapabilitySet::default()
            },
        },
    ) {
        surface::AttachResult::FreshAttached { attachment } => attachment,
        _ => panic!("attach ACP read client"),
    };
    let _subscription = surface.claim_subscription(&attachment.subscription).unwrap();
    let dispatches = surface
        .claim_acp_read_text_file_dispatch(&attachment.client)
        .unwrap();
    let intent = surface_acp_turn_intent(&attachment.baseline.snapshot, "turn-config", "read");
    let mut active = admit_turn_config_operation_with_attachment(
        &mut actor,
        ActionKind::Shell,
        &attachment,
        intent,
    );
    let response = turn_config_response(&active);
    let fence = active.surface_operation.clone().unwrap();
    let response = actor
        .commit_surface_provider_response(&mut active, fence.clone(), &response)
        .unwrap();
    let ProviderStep::ToolCall(request) = &response.response.steps[0] else {
        panic!("expected provider tool call");
    };
    let path = fs::canonicalize(cwd.path()).unwrap().join("turn-tool.txt");
    let (reply, result) = mpsc::sync_channel(1);
    actor.request_surface_acp_read_text_file(
        &mut active,
        fence,
        request.clone(),
        path.clone(),
        None,
        None,
        reply,
    );
    match result.try_recv() {
        Err(mpsc::TryRecvError::Empty) => {}
        result => panic!("ACP read should wait for client content, got {result:?}"),
    }
    let dispatch = dispatches.try_recv().expect("turn-only tool dispatches ACP read");
    assert_eq!(dispatch.path.as_path(), path);
    assert_eq!(dispatch.acp_session_id.as_str(), "turn-config");
    actor
        .claim_surface_acp_read_text_file_write(
            &attachment.client,
            dispatch.call_id.clone(),
            dispatch.capability_revision,
        )
        .unwrap();
    actor
        .mark_surface_acp_read_text_file_written(
            &attachment.client,
            dispatch.call_id.clone(),
            dispatch.capability_revision,
        )
        .unwrap();
    actor
        .settle_surface_acp_read_text_file(
            &attachment.client,
            dispatch.call_id,
            dispatch.capability_revision,
            surface::AcpReadTextFileSettlement::Completed {
                content: "turn-only tool content".into(),
            },
        )
        .unwrap();
    assert_eq!(
        result.recv_timeout(SURFACE_TEST_TIMEOUT).unwrap().unwrap(),
        "turn-only tool content"
    );
    active.generation.join.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn background_response_classifies_external_tools_with_turn_override() {
    for (action, expected) in [
        (ActionKind::Write, surface::SurfaceToolAction::Write),
        (ActionKind::Shell, surface::SurfaceToolAction::Shell),
    ] {
        let cwd = tempfile::tempdir().unwrap();
        let mut actor = turn_config_actor(cwd.path());
        let active = admit_turn_config_operation(&mut actor, action);
        let response = turn_config_response(&active);
        let fence = active.surface_operation.clone().unwrap();
        let background_fence = surface::SurfaceBackgroundFence {
            operation_fence: fence.clone(),
            background_owner_token: surface::SurfaceBackgroundOwnerToken::new([1; 32]),
        };
        let task_id =
            surface::SurfaceTaskId::try_new(active.main_session_task_id.clone().unwrap()).unwrap();
        let task = surface::SurfaceTask {
            task_id: task_id.clone(),
            revision: surface::TaskRevision::try_new(1).unwrap(),
            task_type: surface::SurfaceTaskType::MainSession,
            status: surface::SurfaceTaskStatus::Running,
            backgrounded: true,
            description: surface::DisplayText::new("turn override"),
            created_at: surface::UnixMillis::new(1),
            started_at: None,
            completed_at: None,
            parent_operation: Some(fence.operation_id.clone()),
            parent_task_id: None,
            background_fence: Some(background_fence.clone()),
            workflow_run_id: None,
            subagent_id: None,
            pending_interaction_id: None,
            usage: None,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
        };
        let batch = actor.surface_event_batch_with_commit_id(
            vec![
                (
                    surface::SurfaceScope::Generation {
                        fence: fence.clone(),
                    },
                    surface::SurfaceEvent::Operation(
                        surface::OperationPatch::GenerationTransferred {
                            fence,
                            background_fence: background_fence.clone(),
                            task_id: Some(task_id.clone()),
                        },
                    ),
                ),
                (
                    surface::SurfaceScope::Thread,
                    surface::SurfaceEvent::Task(surface::TaskPatch::Upserted {
                        expected_revision: None,
                        task,
                    }),
                ),
            ],
            None,
        );
        actor
            .resident_surface
            .coordinator
            .commit_actor_batch(&batch)
            .unwrap();
        let typed = TypedProviderBackground {
            fence: background_fence,
            task_id,
            task_registry: active.task_registry.clone(),
            external_tools: active.config.external_tools.clone(),
            outcome: Arc::new(Mutex::new(Some(TypedProviderBackgroundOutcome {
                response: Some(response),
                status: RunStatus::Success,
                error: None,
                usage: None,
                completed_at_ms: 2,
                operation_terminal: None,
            }))),
        };
        let pending = actor
            .prepare_typed_provider_completion(typed, None)
            .unwrap();
        let action = pending
            .completion_batch
            .events
            .as_slice()
            .iter()
            .find_map(|event| match &event.event {
                surface::SurfaceEvent::Tool(surface::ToolPatch::Requested { request }) => {
                    Some(request.action)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(action, expected);
        active.generation.join.await.unwrap();
    }
}
