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
    RuntimeModelResponse::new(
        ProviderResponse {
            steps: vec![ProviderStep::ToolCall(ToolRequest {
                id: "turn-tool-call".into(),
                name: ToolName::External("turn_tool".into()),
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".into()),
            })],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: vec![orca_core::conversation::RawToolCall {
                id: "turn-tool-call".to_string(),
                function_name: "turn_tool".to_string(),
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
